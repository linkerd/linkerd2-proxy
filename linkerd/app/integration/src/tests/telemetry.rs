// The compiler cannot figure out that the `use crate::*`
// import is actually used, and putting the allow attribute on that import in
// particular appears to do nothing... T_T
#![allow(unused_imports)]
use crate::*;
use std::io::Read;

struct Fixture {
    client: client::Client,
    metrics: client::Client,
    proxy: proxy::Listening,
    _profile: controller::ProfileSender,
    dst_tx: Option<controller::DstSender>,
}

struct TcpFixture {
    client: tcp::TcpClient,
    metrics: client::Client,
    proxy: proxy::Listening,
    profile: controller::ProfileSender,
    dst: controller::DstSender,
}

impl Fixture {
    async fn inbound() -> Self {
        info!("running test server");
        Fixture::inbound_with_server(server::new().route("/", "hello").run().await).await
    }

    async fn outbound() -> Self {
        info!("running test server");
        Fixture::outbound_with_server(server::new().route("/", "hello").run().await).await
    }

    async fn inbound_with_server(srv: server::Listening) -> Self {
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(srv.addr, "tele.test.svc.cluster.local");
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.metrics, "localhost");

        let client = client::new(proxy.inbound, "tele.test.svc.cluster.local");
        Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: None,
        }
    }

    async fn outbound_with_server(srv: server::Listening) -> Self {
        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(srv.addr, "tele.test.svc.cluster.local");
        let dest = ctrl.destination_tx(format!("tele.test.svc.cluster.local:{}", srv.addr.port()));
        dest.send_addr(srv.addr);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.metrics, "localhost");

        let client = client::new(proxy.outbound, "tele.test.svc.cluster.local");
        Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: Some(dest),
        }
    }
}

impl TcpFixture {
    const HELLO_MSG: &'static str = "custom tcp hello\n";
    const BYE_MSG: &'static str = "custom tcp bye";

    async fn server() -> server::Listening {
        server::tcp()
            .accept(move |read| {
                assert_eq!(read, Self::HELLO_MSG.as_bytes());
                TcpFixture::BYE_MSG
            })
            .accept(move |read| {
                assert_eq!(read, Self::HELLO_MSG.as_bytes());
                TcpFixture::BYE_MSG
            })
            .run()
            .await
    }

    async fn inbound() -> Self {
        let srv = TcpFixture::server().await;
        let ctrl = controller::new();
        let profile = ctrl.profile_tx_default(srv.addr, &srv.addr.to_string());
        let dst = ctrl.destination_tx(srv.addr.to_string());
        dst.send_addr(srv.addr);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;

        let client = client::tcp(proxy.inbound);
        let metrics = client::http1(proxy.metrics, "localhost");
        TcpFixture {
            client,
            metrics,
            proxy,
            profile,
            dst,
        }
    }

    async fn outbound() -> Self {
        let srv = TcpFixture::server().await;
        let ctrl = controller::new();
        let profile = ctrl.profile_tx_default(srv.addr, &srv.addr.to_string());
        let dst = ctrl.destination_tx(srv.addr.to_string());
        dst.send_addr(srv.addr);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = client::tcp(proxy.outbound);
        let metrics = client::http1(proxy.metrics, "localhost");
        TcpFixture {
            client,
            metrics,
            proxy,
            profile,
            dst,
        }
    }
}

#[tokio::test]
async fn metrics_endpoint_inbound_request_count() {
    let _trace = trace_init();
    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
    } = Fixture::inbound().await;

    // prior to seeing any requests, request count should be empty.
    assert!(!metrics.get("/metrics").await
        .contains("request_total{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\"}"));

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // after seeing a request, the request count should be 1.
    assert_eventually_contains!(metrics.get("/metrics").await, "request_total{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\"} 1");
}

#[tokio::test]
async fn metrics_endpoint_outbound_request_count() {
    let _trace = trace_init();
    let Fixture {
        client,
        metrics,
        proxy,
        _profile,
        dst_tx: _dst_tx,
    } = Fixture::outbound().await;

    let srv_port = proxy.outbound_server.as_ref().unwrap().addr.port();
    let expected = format!("request_total{{direction=\"outbound\",authority=\"tele.test.svc.cluster.local:{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}}", srv_port);
    // prior to seeing any requests, request count should be empty.
    assert!(!metrics.get("/metrics").await.contains(&expected[..]));

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // after seeing a request, the request count should be 1.
    let expected = format!("request_total{{direction=\"outbound\",authority=\"tele.test.svc.cluster.local:{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} 1", srv_port);
    assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);
}

mod response_classification {
    use super::Fixture;
    use crate::*;
    use tracing::info;

    const REQ_STATUS_HEADER: &str = "x-test-status-requested";
    const REQ_GRPC_STATUS_HEADER: &str = "x-test-grpc-status-requested";

    const STATUSES: [http::StatusCode; 6] = [
        http::StatusCode::OK,
        http::StatusCode::NOT_MODIFIED,
        http::StatusCode::BAD_REQUEST,
        http::StatusCode::IM_A_TEAPOT,
        http::StatusCode::GATEWAY_TIMEOUT,
        http::StatusCode::INTERNAL_SERVER_ERROR,
    ];

    fn expected_metric(
        status: &http::StatusCode,
        direction: &str,
        tls: &str,
        no_tls_reason: Option<&str>,
        port: Option<u16>,
    ) -> String {
        let port = if let Some(port) = port {
            format!(":{}", port)
        } else {
            String::new()
        };
        format!(
            "response_total{{direction=\"{}\",authority=\"tele.test.svc.cluster.local{}\",tls=\"{}\",{}status_code=\"{}\",classification=\"{}\"}} 1",
            direction,
            port,
            tls,
            if let Some(reason) = no_tls_reason {
                format!("no_tls_reason=\"{}\",", reason)
            } else {
                String::new()
            },
            status.as_u16(),
            if status.is_server_error() { "failure" } else { "success" },
        )
    }

    async fn make_test_server() -> server::Listening {
        fn parse_header(headers: &http::HeaderMap, which: &str) -> Option<http::StatusCode> {
            headers.get(which).map(|val| {
                val.to_str()
                    .expect("requested status should be ascii")
                    .parse::<http::StatusCode>()
                    .expect("requested status should be numbers")
            })
        }
        info!("running test server");
        server::new()
            .route_fn("/", move |req| {
                let headers = req.headers();
                let status =
                    parse_header(headers, REQ_STATUS_HEADER).unwrap_or(http::StatusCode::OK);
                let grpc_status = parse_header(headers, REQ_GRPC_STATUS_HEADER);
                let mut rsp = if let Some(_grpc_status) = grpc_status {
                    // TODO: tests for grpc statuses
                    unreachable!("not called in test")
                } else {
                    Response::new("".into())
                };
                *rsp.status_mut() = status;
                rsp
            })
            .run()
            .await
    }

    #[tokio::test]
    async fn inbound_http() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy: _proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::inbound_with_server(make_test_server().await).await;

        for (i, status) in STATUSES.iter().enumerate() {
            let request = client
                .request(
                    client
                        .request_builder("/")
                        .header(REQ_STATUS_HEADER, status.as_str())
                        .method("GET"),
                )
                .await
                .unwrap();
            assert_eq!(&request.status(), status);

            for status in &STATUSES[0..i] {
                // assert that the current status code is incremented, *and* that
                // all previous requests are *not* incremented.
                assert_eventually_contains!(
                    metrics.get("/metrics").await,
                    &expected_metric(status, "inbound", "disabled", None, None)
                )
            }
        }
    }

    #[tokio::test]
    async fn outbound_http() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::outbound_with_server(make_test_server().await).await;
        let port = proxy.outbound_server.as_ref().unwrap().addr.port();
        for (i, status) in STATUSES.iter().enumerate() {
            let request = client
                .request(
                    client
                        .request_builder("/")
                        .header(REQ_STATUS_HEADER, status.as_str())
                        .method("GET"),
                )
                .await
                .unwrap();
            assert_eq!(&request.status(), status);

            for status in &STATUSES[0..i] {
                // assert that the current status code is incremented, *and* that
                // all previous requests are *not* incremented.
                assert_eventually_contains!(
                    metrics.get("/metrics").await,
                    &expected_metric(
                        status,
                        "outbound",
                        "no_identity",
                        Some("not_provided_by_service_discovery"),
                        Some(port)
                    )
                )
            }
        }
    }
}

// Ignore this test on CI, because our method of adding latency to requests
// (calling `thread::sleep`) is likely to be flakey on Travis.
// Eventually, we can add some kind of mock timer system for simulating latency
// more reliably, and re-enable this test.
#[tokio::test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
async fn metrics_endpoint_inbound_response_latency() {
    let _trace = trace_init();

    info!("running test server");
    let srv = server::new()
        .route_with_latency("/hey", "hello", Duration::from_millis(500))
        .route_with_latency("/hi", "good morning", Duration::from_millis(40))
        .run()
        .await;

    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
    } = Fixture::inbound_with_server(srv).await;

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // assert the >=1000ms bucket is incremented by our request with 500ms
    // extra latency.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"1000\"} 1");
    // the histogram's count should be 1.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 1");
    // TODO: we're not going to make any assertions about the
    // response_latency_ms_sum stat, since its granularity depends on the actual
    // observed latencies, which may vary a bit. we could make more reliable
    // assertions about that stat if we were using a mock timer, though, as the
    // observed latency values would be predictable.

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"50\"} 1");
    // 1000ms bucket should be incremented as well, since it counts *all*
    // observations less than or equal to 1000ms, even if they also increment
    // other buckets.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"1000\"} 2");
    // the histogram's total count should be 2.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 2");

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"50\"} 2");
    // 1000ms bucket should be incremented as well.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"1000\"} 3");
    // the histogram's total count should be 3.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 3");

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // 50ms bucket should be un-changed by the request with 500ms latency.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"50\"} 2");
    // 1000ms bucket should be incremented.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\",le=\"1000\"} 4");
    // the histogram's total count should be 4.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 4");
}

// Ignore this test on CI, because our method of adding latency to requests
// (calling `thread::sleep`) is likely to be flakey on Travis.
// Eventually, we can add some kind of mock timer system for simulating latency
// more reliably, and re-enable this test.
#[tokio::test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
async fn metrics_endpoint_outbound_response_latency() {
    let _trace = trace_init();

    info!("running test server");
    let srv = server::new()
        .route_with_latency("/hey", "hello", Duration::from_millis(500))
        .route_with_latency("/hi", "good morning", Duration::from_millis(40))
        .run()
        .await;

    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
    } = Fixture::outbound_with_server(srv).await;

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // assert the >=1000ms bucket is incremented by our request with 500ms
    // extra latency.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"1000\"} 1");
    // the histogram's count should be 1.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
    // TODO: we're not going to make any assertions about the
    // response_latency_ms_sum stat, since its granularity depends on the actual
    // observed latencies, which may vary a bit. we could make more reliable
    // assertions about that stat if we were using a mock timer, though, as the
    // observed latency values would be predictable.

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"50\"} 1");
    // 1000ms bucket should be incremented as well, since it counts *all*
    // bservations less than or equal to 1000ms, even if they also increment
    // other buckets.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"1000\"} 2");
    // the histogram's total count should be 2.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 2");

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"50\"} 2");
    // 1000ms bucket should be incremented as well.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"1000\"} 3");
    // the histogram's total count should be 3.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 3");

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // 50ms bucket should be un-changed by the request with 500ms latency.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"50\"} 2");
    // 1000ms bucket should be incremented.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_bucket{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",le=\"1000\"} 4");
    // the histogram's total count should be 4.
    assert_eventually_contains!(metrics.get("/metrics").await,
        "response_latency_ms_count{direction=\"outbound\",authority=\"tele.test.svc.cluster.local\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 4");
}

// Tests for destination labels provided by control plane service discovery.
mod outbound_dst_labels {
    use super::Fixture;
    use crate::*;
    use controller::DstSender;

    async fn fixture(dest: &str) -> (Fixture, SocketAddr) {
        info!("running test server");
        let srv = server::new().route("/", "hello").run().await;

        let addr = srv.addr;

        let ctrl = controller::new();
        let _profile = ctrl.profile_tx_default(addr, dest);
        let dst_tx = ctrl.destination_tx(format!("{}:{}", dest, addr.port()));

        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.metrics, "localhost");

        let client = client::new(proxy.outbound, dest);

        let f = Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: Some(dst_tx),
        };

        (f, addr)
    }

    #[tokio::test]
    async fn multiple_addr_labels() {
        let _trace = trace_init();
        let (
            Fixture {
                client,
                metrics,
                proxy: _proxy,
                _profile,
                dst_tx,
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();
        {
            let mut labels = HashMap::new();
            labels.insert("addr_label1".to_owned(), "foo".to_owned());
            labels.insert("addr_label2".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, labels, HashMap::new());
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // We can't make more specific assertions about the metrics
        // besides asserting that both labels are present somewhere in the
        // scrape, because testing for whole metric lines would depend on
        // the order in which the labels occur, and we can't depend on hash
        // map ordering.
        assert_eventually_contains!(metrics.get("/metrics").await, "dst_addr_label1=\"foo\"");
        assert_eventually_contains!(metrics.get("/metrics").await, "dst_addr_label2=\"bar\"");
    }

    #[tokio::test]
    async fn multiple_addrset_labels() {
        let _trace = trace_init();
        let (
            Fixture {
                client,
                metrics,
                proxy: _proxy,
                _profile,
                dst_tx,
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();

        {
            let mut labels = HashMap::new();
            labels.insert("set_label1".to_owned(), "foo".to_owned());
            labels.insert("set_label2".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, HashMap::new(), labels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // We can't make more specific assertions about the metrics
        // besides asserting that both labels are present somewhere in the
        // scrape, because testing for whole metric lines would depend on
        // the order in which the labels occur, and we can't depend on hash
        // map ordering.
        assert_eventually_contains!(metrics.get("/metrics").await, "dst_set_label1=\"foo\"");
        assert_eventually_contains!(metrics.get("/metrics").await, "dst_set_label2=\"bar\"");
    }

    #[tokio::test]
    async fn labeled_addr_and_addrset() {
        let _trace = trace_init();
        let (
            Fixture {
                client,
                metrics,
                proxy,
                _profile,
                dst_tx,
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();
        let auth = format!(
            "labeled.test.svc.cluster.local:{}",
            proxy.outbound_server.as_ref().unwrap().addr.port()
        );

        {
            let mut alabels = HashMap::new();
            alabels.insert("addr_label".to_owned(), "foo".to_owned());
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        let expected = format!(
            "response_latency_ms_count{{direction=\"outbound\",authority=\"{}\",dst_addr_label=\"foo\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"}} 1",
            auth
        );
        assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);

        let expected =             format!(
            "request_total{{direction=\"outbound\",authority=\"{}\",dst_addr_label=\"foo\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} 1",
            auth
        );
        assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);

        let expected = format!(
            "response_total{{direction=\"outbound\",authority=\"{}\",dst_addr_label=\"foo\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"}} 1",
            auth
        );
        assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);
    }

    // Ignore this test on CI, as it may fail due to the reduced concurrency
    // on CI containers causing the proxy to see both label updates from
    // the mock controller before the first request has finished.
    // See linkerd/linkerd2#751
    #[tokio::test]
    #[cfg_attr(not(feature = "flaky_tests"), ignore)]
    async fn controller_updates_addr_labels() {
        let _trace = trace_init();
        info!("running test server");

        let (
            Fixture {
                client,
                metrics,
                proxy: _proxy,
                _profile,
                dst_tx,
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();
        {
            let mut alabels = HashMap::new();
            alabels.insert("addr_label".to_owned(), "foo".to_owned());
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "unchanged".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the first request should be labeled with `dst_addr_label="foo"`
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");

        {
            let mut alabels = HashMap::new();
            alabels.insert("addr_label".to_owned(), "bar".to_owned());
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "unchanged".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the second request should increment stats labeled with `dst_addr_label="bar"`
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"bar\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"bar\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"bar\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");
        // stats recorded from the first request should still be present.
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_addr_label=\"foo\",dst_set_label=\"unchanged\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");
    }

    // Ignore this test on CI, as it may fail due to the reduced concurrency
    // on CI containers causing the proxy to see both label updates from
    // the mock controller before the first request has finished.
    // See linkerd/linkerd2#751
    #[tokio::test]
    #[cfg_attr(not(feature = "flaky_tests"), ignore)]
    async fn controller_updates_set_labels() {
        let _trace = trace_init();
        info!("running test server");
        let (
            Fixture {
                client,
                metrics,
                proxy: _proxy,
                _profile,
                dst_tx,
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();
        {
            let alabels = HashMap::new();
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "foo".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the first request should be labeled with `dst_addr_label="foo"`
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");

        {
            let alabels = HashMap::new();
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the second request should increment stats labeled with `dst_addr_label="bar"`
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"bar\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");
        // stats recorded from the first request should still be present.
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_latency_ms_count{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "request_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"} 1");
        assert_eventually_contains!(metrics.get("/metrics").await,
            "response_total{direction=\"outbound\",authority=\"labeled.test.svc.cluster.local\",dst_set_label=\"foo\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",status_code=\"200\",classification=\"success\"} 1");
    }
}

#[tokio::test]
async fn metrics_have_no_double_commas() {
    // Test for regressions to linkerd/linkerd2#600.
    let _trace = trace_init();

    info!("running test server");
    let inbound_srv = server::new().route("/hey", "hello").run().await;
    let outbound_srv = server::new().route("/hey", "hello").run().await;

    let ctrl = controller::new();
    let _profile_in =
        ctrl.profile_tx_default("tele.test.svc.cluster.local", "tele.test.svc.cluster.local");
    let _profile_out = ctrl.profile_tx_default(outbound_srv.addr, "tele.test.svc.cluster.local");
    let out_dest = ctrl.destination_tx(format!(
        "tele.test.svc.cluster.local:{}",
        outbound_srv.addr.port()
    ));
    out_dest.send_addr(outbound_srv.addr);
    let in_dest = ctrl.destination_tx(format!(
        "tele.test.svc.cluster.local:{}",
        inbound_srv.addr.port()
    ));
    in_dest.send_addr(inbound_srv.addr);
    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .inbound(inbound_srv)
        .outbound(outbound_srv)
        .run()
        .await;
    let client = client::new(proxy.inbound, "tele.test.svc.cluster.local");
    let metrics = client::http1(proxy.metrics, "localhost");

    let scrape = metrics.get("/metrics").await;
    assert!(!scrape.contains(",,"));

    info!("inbound.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    let scrape = metrics.get("/metrics").await;
    assert!(!scrape.contains(",,"), "inbound metrics had double comma");

    let client = client::new(proxy.outbound, "tele.test.svc.cluster.local");

    info!("outbound.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    let scrape = metrics.get("/metrics").await;
    assert!(!scrape.contains(",,"), "outbound metrics had double comma");
}

#[tokio::test]
async fn metrics_has_start_time() {
    let Fixture {
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        ..
    } = Fixture::inbound().await;
    let uptime_regex = regex::Regex::new(r"process_start_time_seconds \d+")
        .expect("compiling regex shouldn't fail");
    assert_eventually!(uptime_regex.find(&metrics.get("/metrics").await).is_some())
}

mod transport {
    use super::*;
    use crate::*;

    #[tokio::test]
    async fn inbound_http_accept() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::inbound().await;

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\"} 1"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 1"
        );

        // create a new client to force a new connection
        let client = client::new(proxy.inbound, "tele.test.svc.cluster.local");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\"} 2"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 2"
        );
    }

    #[tokio::test]
    async fn inbound_http_connect() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::inbound().await;

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );

        // create a new client to force a new connection
        let client = client::new(proxy.inbound, "tele.test.svc.cluster.local");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // server connection should be pooled
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );
    }

    #[tokio::test]
    async fn outbound_http_accept() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::outbound().await;

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1"
        );

        // create a new client to force a new connection
        let client = client::new(proxy.outbound, "tele.test.svc.cluster.local");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 2"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 2"
        );
    }

    #[tokio::test]
    async fn outbound_http_connect() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::outbound().await;
        let expected = format!(
            "tcp_open_total{{peer=\"dst\",direction=\"outbound\",authority=\"tele.test.svc.cluster.local:{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} 1",
            proxy.outbound_server.as_ref().unwrap().addr.port(),
        );
        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);

        // create a new client to force a new connection
        let client2 = client::new(proxy.outbound, "tele.test.svc.cluster.local");

        info!("client.get(/)");
        assert_eq!(client2.get("/").await, "hello");
        // server connection should be pooled
        assert_eventually_contains!(metrics.get("/metrics").await, &expected[..]);
    }

    #[tokio::test]
    async fn inbound_tcp_connect() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::inbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_open_total{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1");
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn inbound_tcp_connect_err() {
        let _trace = trace_init();
        let srv = tcp::server()
            .accept_fut(move |sock| {
                drop(sock);
                future::ok(())
            })
            .run()
            .await;
        let proxy = proxy::new().inbound(srv).run().await;

        let client = client::tcp(proxy.inbound);
        let metrics = client::http1(proxy.metrics, "localhost");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, &[]);
        // Connection to the server should be a failure with the EXFULL error
        // code.
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"EXFULL\"} 1");
        // Connection from the client should have closed cleanly.
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 1"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn outbound_tcp_connect_err() {
        let _trace = trace_init();
        let srv = tcp::server()
            .accept_fut(move |sock| {
                drop(sock);
                future::ok(())
            })
            .run()
            .await;
        let proxy = proxy::new().outbound(srv).run().await;

        let client = client::tcp(proxy.outbound);
        let metrics = client::http1(proxy.metrics, "localhost");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, &[]);
        // Connection to the server should be a failure with the EXFULL error
        // code.
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"dst\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"EXFULL\"} 1");
        // Connection from the client should have closed cleanly.
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");
    }

    #[tokio::test]
    async fn inbound_tcp_accept() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::inbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());

        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\"} 1"
        );

        tcp_client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 1"
        );

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());

        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\"} 2"
        );
        tcp_client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 2"
        );
    }

    // linkerd/linkerd2#831
    #[tokio::test]
    #[cfg_attr(not(feature = "flaky_tests"), ignore)]
    async fn inbound_tcp_duration() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::inbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;
        // TODO: make assertions about buckets
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 1");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 1");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");

        tcp_client.shutdown().await;
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"inbound\",tls=\"disabled\",errno=\"\"} 2");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 2");
    }

    #[tokio::test]
    async fn inbound_tcp_write_bytes_total() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::inbound().await;
        let src_expected = format!(
            "tcp_write_bytes_total{{peer=\"src\",direction=\"inbound\",tls=\"disabled\"}} {}",
            TcpFixture::BYE_MSG.len()
        );
        let dst_expected = format!(
            "tcp_write_bytes_total{{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"}} {}",
            TcpFixture::HELLO_MSG.len()
        );

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out, &src_expected);
        assert_eventually_contains!(out, &dst_expected);
    }

    #[tokio::test]
    async fn inbound_tcp_read_bytes_total() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::inbound().await;
        let src_expected = format!(
            "tcp_read_bytes_total{{peer=\"src\",direction=\"inbound\",tls=\"disabled\"}} {}",
            TcpFixture::HELLO_MSG.len()
        );
        let dst_expected = format!(
            "tcp_read_bytes_total{{peer=\"dst\",direction=\"inbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"}} {}",
            TcpFixture::BYE_MSG.len()
        );

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out, &src_expected);
        assert_eventually_contains!(out, &dst_expected);
    }

    #[tokio::test]
    async fn outbound_tcp_connect() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::outbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        let expected = format!(
            "tcp_open_total{{peer=\"dst\",direction=\"outbound\",authority=\"{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} 1",
            proxy.outbound_server.as_ref().unwrap().addr,
        );
        assert_eventually_contains!(metrics.get("/metrics").await, &expected);
    }

    #[tokio::test]
    async fn outbound_tcp_accept() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::outbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());

        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );

        tcp_client.shutdown().await;
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());

        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 2"
        );
        tcp_client.shutdown().await;
        assert_eventually_contains!(metrics.get("/metrics").await,
            "tcp_close_total{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 2");
    }

    #[tokio::test]
    #[cfg_attr(not(feature = "flaky_tests"), ignore)]
    async fn outbound_tcp_duration() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::outbound().await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;
        // TODO: make assertions about buckets
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"\"} 1");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 1");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"\"} 1");

        tcp_client.shutdown().await;
        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\",errno=\"\"} 2");
        assert_eventually_contains!(out,
            "tcp_connection_duration_ms_count{peer=\"dst\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\",errno=\"\"} 2");
    }

    #[tokio::test]
    async fn outbound_tcp_write_bytes_total() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::outbound().await;
        let src_expected = format!(
            "tcp_write_bytes_total{{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"}} {}",
            TcpFixture::BYE_MSG.len()
        );
        let dst_expected = format!(
            "tcp_write_bytes_total{{peer=\"dst\",direction=\"outbound\",authority=\"{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} {}",
            proxy.outbound_server.as_ref().unwrap().addr,
            TcpFixture::HELLO_MSG.len()
        );

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out, &src_expected);
        assert_eventually_contains!(out, &dst_expected);
    }

    #[tokio::test]
    async fn outbound_tcp_read_bytes_total() {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy,
            dst: _dst,
            profile: _profile,
        } = TcpFixture::outbound().await;

        let src_expected = format!(
            "tcp_read_bytes_total{{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"}} {}",
            TcpFixture::HELLO_MSG.len()
        );
        let dst_expected = format!(
            "tcp_read_bytes_total{{peer=\"dst\",direction=\"outbound\",authority=\"{}\",tls=\"no_identity\",no_tls_reason=\"not_provided_by_service_discovery\"}} {}",
            proxy.outbound_server.as_ref().unwrap().addr,
            TcpFixture::BYE_MSG.len()
        );

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        let out = metrics.get("/metrics").await;
        assert_eventually_contains!(out, &src_expected);
        assert_eventually_contains!(out, &dst_expected);
    }

    #[tokio::test]
    async fn outbound_tcp_open_connections() {
        let _trace = trace_init();
        let fixture = TcpFixture::outbound().await;
        let client = fixture.client;
        let metrics = fixture.metrics;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );
        tcp_client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 0"
        );
        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );

        tcp_client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 0"
        );
    }

    #[tokio::test]
    async fn outbound_http_tcp_open_connections() {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: _dst_tx,
        } = Fixture::outbound().await;

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");

        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 0"
        );

        // create a new client to force a new connection
        let client = client::new(proxy.outbound, "tele.test.svc.cluster.local");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 1"
        );
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        assert_eventually_contains!(
            metrics.get("/metrics").await,
            "tcp_open_connections{peer=\"src\",direction=\"outbound\",tls=\"no_identity\",no_tls_reason=\"loopback\"} 0"
        );
    }
}

// linkerd/linkerd2#613
#[tokio::test]
async fn metrics_compression() {
    let _trace = trace_init();

    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
    } = Fixture::inbound().await;

    let do_scrape = |encoding: &str| {
        let req = metrics.request(
            metrics
                .request_builder("/metrics")
                .method("GET")
                .header("Accept-Encoding", encoding),
        );

        let encoding = encoding.to_owned();
        async move {
            let resp = req.await.expect("scrape");

            {
                // create a new scope so we can release our borrow on `resp` before
                // getting the body
                let content_encoding = resp.headers().get("content-encoding").as_ref().map(|val| {
                    val.to_str()
                        .expect("content-encoding value should be ascii")
                });
                assert_eq!(
                    content_encoding,
                    Some("gzip"),
                    "unexpected Content-Encoding {:?} (requested Accept-Encoding: {})",
                    content_encoding,
                    encoding.as_str()
                );
            }

            let mut body = hyper::body::aggregate(resp.into_body())
                .await
                .expect("response body concat");
            let mut decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(
                body.copy_to_bytes(body.remaining()),
            ));
            let mut scrape = String::new();
            decoder.read_to_string(&mut scrape).unwrap_or_else(|_| {
                panic!("decode gzip (requested Accept-Encoding: {})", encoding)
            });
            scrape
        }
    };

    let encodings = &[
        "gzip",
        "deflate, gzip",
        "gzip,deflate",
        "brotli,gzip,deflate",
    ];

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    for &encoding in encodings {
        assert_eventually_contains!(do_scrape(encoding).await,
            "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 1");
    }

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    for &encoding in encodings {
        assert_eventually_contains!(do_scrape(encoding).await,
            "response_latency_ms_count{direction=\"inbound\",authority=\"tele.test.svc.cluster.local\",tls=\"disabled\",status_code=\"200\"} 2");
    }
}
