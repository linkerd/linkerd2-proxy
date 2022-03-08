// The compiler cannot figure out that the `use crate::*`
// import is actually used, and putting the allow attribute on that import in
// particular appears to do nothing... T_T
#![allow(unused_imports)]

mod tcp_errors;

use crate::*;
use std::io::Read;

struct Fixture {
    client: client::Client,
    metrics: client::Client,
    proxy: proxy::Listening,
    _profile: controller::ProfileSender,
    dst_tx: Option<controller::DstSender>,
    labels: metrics::Labels,
    tcp_src_labels: metrics::Labels,
    tcp_dst_labels: metrics::Labels,
}

struct TcpFixture {
    client: tcp::TcpClient,
    metrics: client::Client,
    proxy: proxy::Listening,
    profile: controller::ProfileSender,
    dst: controller::DstSender,
    src_labels: metrics::Labels,
    dst_labels: metrics::Labels,
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
        let orig_dst = srv.addr;
        let _profile = ctrl.profile_tx_default(orig_dst, "tele.test.svc.cluster.local");
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.admin, "localhost");

        let client = client::new(proxy.inbound, "tele.test.svc.cluster.local");
        let tcp_dst_labels = metrics::labels().label("direction", "inbound");
        let tcp_src_labels = tcp_dst_labels.clone().label("target_addr", orig_dst);
        let labels = tcp_dst_labels
            .clone()
            .label("authority", "tele.test.svc.cluster.local");
        let tcp_src_labels = tcp_src_labels.label("peer", "src");
        let tcp_dst_labels = tcp_dst_labels.label("peer", "dst");
        Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: None,
            labels,
            tcp_src_labels,
            tcp_dst_labels,
        }
    }

    async fn outbound_with_server(srv: server::Listening) -> Self {
        let ctrl = controller::new();
        let orig_dst = srv.addr;
        let _profile = ctrl.profile_tx_default(orig_dst, "tele.test.svc.cluster.local");
        let authority = format!("tele.test.svc.cluster.local:{}", orig_dst.port());
        let dest = ctrl.destination_tx(authority.clone());
        dest.send_addr(srv.addr);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.admin, "localhost");

        let client = client::new(proxy.outbound, "tele.test.svc.cluster.local");
        let tcp_labels = metrics::labels()
            .label("direction", "outbound")
            .label("target_addr", orig_dst);
        let labels = tcp_labels.clone().label("authority", authority);
        let tcp_src_labels = tcp_labels.clone().label("peer", "src");
        let tcp_dst_labels = tcp_labels.label("peer", "dst");
        Fixture {
            client,
            metrics,
            proxy,
            _profile,
            dst_tx: Some(dest),
            labels,
            tcp_src_labels,
            tcp_dst_labels,
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
        let orig_dst = srv.addr;
        let profile = ctrl.profile_tx_default(orig_dst, &orig_dst.to_string());
        let dst = ctrl.destination_tx(orig_dst.to_string());
        dst.send_addr(orig_dst);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .inbound(srv)
            .run()
            .await;

        let client = client::tcp(proxy.inbound);
        let metrics = client::http1(proxy.admin, "localhost");

        let src_labels = metrics::labels()
            .label("direction", "inbound")
            .label("peer", "src")
            .label("target_addr", orig_dst)
            .label("srv_name", "default:all-unauthenticated");

        let dst_labels = metrics::labels()
            .label("direction", "inbound")
            .label("peer", "dst")
            .label("tls", "no_identity")
            .label("no_tls_reason", "loopback");
        TcpFixture {
            client,
            metrics,
            proxy,
            profile,
            dst,
            src_labels,
            dst_labels,
        }
    }

    async fn outbound() -> Self {
        let srv = TcpFixture::server().await;
        let ctrl = controller::new();
        let orig_dst = srv.addr;
        let profile = ctrl.profile_tx_default(orig_dst, &orig_dst.to_string());
        let dst = ctrl.destination_tx(orig_dst.to_string());
        dst.send_addr(orig_dst);
        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;

        let client = client::tcp(proxy.outbound);
        let metrics = client::http1(proxy.admin, "localhost");

        let src_labels = metrics::labels()
            .label("direction", "outbound")
            .label("peer", "src")
            .label("tls", "no_identity")
            .label("no_tls_reason", "loopback")
            .label("target_addr", orig_dst);
        let dst_labels = metrics::labels()
            .label("direction", "outbound")
            .label("peer", "dst");

        TcpFixture {
            client,
            metrics,
            proxy,
            profile,
            dst,
            src_labels,
            dst_labels,
        }
    }
}

#[tokio::test]
async fn admin_request_count() {
    let _trace = trace_init();
    let fixture = Fixture::inbound().await;
    let metrics = fixture.metrics;
    let metric = metrics::metric("request_total")
        .label("direction", "inbound")
        .label("target_addr", metrics.target_addr())
        .value(1usize);

    // We can't assert that the metric is not present, since `GET /metrics`
    // will bump the request count, lol
    metric.assert_in(&metrics).await;
}

#[tokio::test]
async fn admin_transport_metrics() {
    let _trace = trace_init();
    let fixture = Fixture::inbound().await;
    let metrics = fixture.metrics;
    let labels = metrics::labels()
        .label("direction", "inbound")
        .label("target_addr", metrics.target_addr())
        .label("peer", "src");

    let mut open_total = labels.metric("tcp_open_total").value(1usize);
    open_total.assert_in(&metrics).await;
    assert!(
        open_total
            .clone()
            .label("peer", "dst")
            .is_not_in(metrics.get("/metrics").await),
        "peer=\"dst\" metrics don't make sense for the admin server"
    );

    let mut close_total = labels.metric("tcp_close_total");
    assert!(
        close_total.is_not_in(metrics.get("/metrics").await),
        "client connection hasn't been closed yet"
    );

    // Close the connection and reconnect.
    let metrics = metrics.shutdown().await.reconnect();

    close_total.set_value(1).assert_in(&metrics).await;
    assert!(
        close_total
            .label("peer", "dst")
            .is_not_in(metrics.get("/metrics").await),
        "peer=\"dst\" metrics don't make sense for the admin server"
    );
    open_total.set_value(2usize).assert_in(&metrics).await;

    let open_gauge = labels.metric("tcp_open_connections");
    open_gauge.clone().value(1usize).assert_in(&metrics).await;
    assert!(
        open_gauge
            .label("peer", "dst")
            .is_not_in(metrics.get("/metrics").await),
        "peer=\"dst\" metrics don't make sense for the admin server"
    );
}

#[tokio::test]
async fn metrics_endpoint_inbound_request_count() {
    test_http_count("request_total", Fixture::inbound()).await;
}

#[tokio::test]
async fn metrics_endpoint_outbound_request_count() {
    test_http_count("request_total", Fixture::outbound()).await
}

#[tokio::test]
async fn metrics_endpoint_inbound_response_count() {
    test_http_count("response_total", Fixture::inbound()).await;
}

#[tokio::test]
async fn metrics_endpoint_outbound_response_count() {
    test_http_count("response_total", Fixture::outbound()).await
}

async fn test_http_count(metric: &str, fixture: impl Future<Output = Fixture>) {
    let _trace = trace_init();
    let Fixture {
        client,
        metrics,
        proxy: _proxy,
        _profile,
        dst_tx: _dst_tx,
        labels,
        ..
    } = fixture.await;

    let metric = labels.metric(metric);

    assert!(metric.is_not_in(metrics.get("/metrics").await));

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    // after seeing a request, the request count should be 1.
    metric.value(1u64).assert_in(&metrics).await;
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

    async fn test_http(fixture: impl Future<Output = Fixture>) {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy: _proxy,
            _profile,
            dst_tx: _dst_tx,
            labels,
            ..
        } = fixture.await;

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
                labels
                    .metric("response_total")
                    .label("status_code", status.as_u16())
                    .label(
                        "classification",
                        if status.is_server_error() {
                            "failure"
                        } else {
                            "success"
                        },
                    )
                    .value(1u64)
                    .assert_in(&metrics)
                    .await;
            }
        }
    }

    #[tokio::test]
    async fn inbound_http() {
        let fixture = async { Fixture::inbound_with_server(make_test_server().await).await };
        test_http(fixture).await
    }

    #[tokio::test]
    async fn outbound_http() {
        let fixture = async { Fixture::outbound_with_server(make_test_server().await).await };
        test_http(fixture).await
    }
}

async fn test_response_latency<F>(mk_fixture: impl Fn(server::Listening) -> F)
where
    F: Future<Output = Fixture>,
{
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
        labels,
        ..
    } = mk_fixture(srv).await;

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // assert the >=1000ms bucket is incremented by our request with 500ms
    // extra latency.
    let labels = labels.label("status_code", 200);
    let mut bucket_1000 = labels
        .clone()
        .metric("response_latency_ms_bucket")
        .label("le", 1000)
        .value(1u64);
    let mut bucket_50 = labels.metric("response_latency_ms_bucket").label("le", 50);
    let mut count = labels.metric("response_latency_ms_count").value(1u64);

    bucket_1000.assert_in(&metrics).await;
    // the histogram's count should be 1.
    count.assert_in(&metrics).await;
    // TODO: we're not going to make any assertions about the
    // response_latency_ms_sum stat, since its granularity depends on the actual
    // observed latencies, which may vary a bit. we could make more reliable
    // assertions about that stat if we were using a mock timer, though, as the
    // observed latency values would be predictable.

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    bucket_50.set_value(1u64).assert_in(&metrics).await;
    // 1000ms bucket should be incremented as well, since it counts *all*
    // observations less than or equal to 1000ms, even if they also increment
    // other buckets.
    bucket_1000.set_value(2u64).assert_in(&metrics).await;
    // the histogram's total count should be 2.
    count.set_value(2u64).assert_in(&metrics).await;

    info!("client.get(/hi)");
    assert_eq!(client.get("/hi").await, "good morning");

    // request with 40ms extra latency should fall into the 50ms bucket.
    bucket_50.set_value(2u64).assert_in(&metrics).await;
    // 1000ms bucket should be incremented as well.
    bucket_1000.set_value(3).assert_in(&metrics).await;
    // the histogram's total count should be 3.
    count.set_value(3).assert_in(&metrics).await;

    info!("client.get(/hey)");
    assert_eq!(client.get("/hey").await, "hello");

    // 50ms bucket should be un-changed by the request with 500ms latency.
    bucket_50.assert_in(&metrics).await;
    // 1000ms bucket should be incremented.
    bucket_1000.set_value(4).assert_in(&metrics).await;
    // the histogram's total count should be 4.
    count.set_value(4).assert_in(&metrics).await;
}

// Ignore this test on CI, because our method of adding latency to requests
// (calling `thread::sleep`) is likely to be flakey on Travis.
// Eventually, we can add some kind of mock timer system for simulating latency
// more reliably, and re-enable this test.
#[tokio::test]
#[cfg_attr(not(feature = "flakey-in-ci"), ignore)]
async fn inbound_response_latency() {
    test_response_latency(Fixture::inbound_with_server).await
}

// Ignore this test on CI, because our method of adding latency to requests
// (calling `thread::sleep`) is likely to be flakey on Travis.
// Eventually, we can add some kind of mock timer system for simulating latency
// more reliably, and re-enable this test.
#[tokio::test]
#[cfg_attr(not(feature = "flakey-in-ci"), ignore)]
async fn outbound_response_latency() {
    test_response_latency(Fixture::outbound_with_server).await
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
        let dest_and_port = format!("{}:{}", dest, addr.port());
        let dst_tx = ctrl.destination_tx(dest_and_port.clone());

        let proxy = proxy::new()
            .controller(ctrl.run().await)
            .outbound(srv)
            .run()
            .await;
        let metrics = client::http1(proxy.admin, "localhost");

        let client = client::new(proxy.outbound, dest);
        let tcp_labels = metrics::labels()
            .label("direction", "outbound")
            .label("target_addr", addr);
        let labels = tcp_labels.clone().label("authority", dest_and_port);
        let f = Fixture {
            client,
            metrics,
            proxy,
            _profile,
            labels,
            tcp_src_labels: tcp_labels.clone(),
            tcp_dst_labels: tcp_labels,
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
                labels,
                ..
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

        let labels = labels
            .label("dst_addr_label1", "foo")
            .label("dst_addr_label2", "bar");

        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels.metric(metric).assert_in(&metrics).await;
        }
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
                labels,
                ..
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

        let labels = labels
            .label("dst_set_label1", "foo")
            .label("dst_set_label2", "bar");

        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels.metric(metric).assert_in(&metrics).await;
        }
    }

    #[tokio::test]
    async fn labeled_addr_and_addrset() {
        let _trace = trace_init();
        let (
            Fixture {
                client,
                metrics,
                proxy: _proxy,
                _profile,
                dst_tx,
                labels,
                ..
            },
            addr,
        ) = fixture("labeled.test.svc.cluster.local").await;
        let dst_tx = dst_tx.unwrap();

        {
            let mut alabels = HashMap::new();
            alabels.insert("addr_label".to_owned(), "foo".to_owned());
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");

        let labels = labels
            .label("dst_addr_label", "foo")
            .label("dst_set_label", "bar");

        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels.metric(metric).assert_in(&metrics).await;
        }
    }

    // Ignore this test on CI, as it may fail due to the reduced concurrency
    // on CI containers causing the proxy to see both label updates from
    // the mock controller before the first request has finished.
    // See linkerd/linkerd2#751
    #[tokio::test]
    #[cfg_attr(not(feature = "flakey-in-ci"), ignore)]
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
                labels,
                ..
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

        let labels1 = labels
            .clone()
            .label("dst_addr_label", "foo")
            .label("dst_set_label", "unchanged");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");

        // the first request should be labeled with `dst_addr_label="foo"`
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels1.metric(metric).value(1u64).assert_in(&metrics).await;
        }

        {
            let mut alabels = HashMap::new();
            alabels.insert("addr_label".to_owned(), "bar".to_owned());
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "unchanged".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }

        let labels2 = labels
            .label("dst_addr_label", "bar")
            .label("dst_set_label", "unchanged");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");

        // the second request should increment stats labeled with `dst_addr_label="bar"`
        // the first request should be labeled with `dst_addr_label="foo"`
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels1.metric(metric).value(1u64).assert_in(&metrics).await;
        }

        // stats recorded from the first request should still be present.
        // the first request should be labeled with `dst_addr_label="foo"`
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels2.metric(metric).value(1u64).assert_in(&metrics).await;
        }
    }

    // FIXME(ver) this test was marked flakey, but now it consistently fails.
    #[ignore]
    #[tokio::test]
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
                labels,
                ..
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
        let labels1 = labels.clone().label("dst_set_label", "foo");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the first request should be labeled with `dst_addr_label="foo"
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels1.metric(metric).value(1u64).assert_in(&client).await;
        }

        {
            let alabels = HashMap::new();
            let mut slabels = HashMap::new();
            slabels.insert("set_label".to_owned(), "bar".to_owned());
            dst_tx.send_labeled(addr, alabels, slabels);
        }
        let labels2 = labels.label("dst_set_label", "bar");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // the second request should increment stats labeled with `dst_addr_label="bar"`
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels2.metric(metric).value(1u64).assert_in(&metrics).await;
        }
        // stats recorded from the first request should still be present.
        for &metric in &[
            "request_total",
            "response_total",
            "response_latency_ms_count",
        ] {
            labels1.metric(metric).value(1u64).assert_in(&metrics).await;
        }
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
    let metrics = client::http1(proxy.admin, "localhost");

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

    async fn test_http_connect(
        fixture: impl Future<Output = Fixture>,
        extra_labels: metrics::Labels,
    ) {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy: _proxy,
            _profile,
            dst_tx: _dst_tx,
            tcp_dst_labels,
            ..
        } = fixture.await;

        let labels = tcp_dst_labels.and(extra_labels);
        let opens = labels.metric("tcp_open_total").value(1u64);

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        opens.assert_in(&metrics).await;

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        // Pooled connection doesn't increment the metric.
        opens.assert_in(&metrics).await;
    }

    async fn test_http_accept(
        fixture: impl Future<Output = Fixture>,
        extra_labels: metrics::Labels,
    ) {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy: _proxy,
            _profile,
            dst_tx: _dst_tx,
            tcp_src_labels,
            ..
        } = fixture.await;

        let labels = tcp_src_labels.and(extra_labels);
        let mut opens = labels.metric("tcp_open_total").value(1u64);
        let mut closes = labels
            .metric("tcp_close_total")
            .label("errno", "")
            .value(1u64);
        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        opens.assert_in(&metrics).await;
        // Shut down the client to force the connection to close.
        let new_client = client.shutdown().await;
        closes.assert_in(&metrics).await;

        // create a new client to force a new connection
        let client = new_client.reconnect();

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        opens.set_value(2u64).assert_in(&metrics).await;
        // Shut down the client to force the connection to close.
        client.shutdown().await;
        closes.set_value(2u64).assert_in(&metrics).await;
    }

    async fn test_tcp_connect(fixture: impl Future<Output = TcpFixture>) {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
            dst_labels,
            ..
        } = fixture.await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        dst_labels
            .metric("tcp_open_total")
            .value(1u64)
            .assert_in(&metrics)
            .await;
    }

    async fn test_tcp_accept(fixture: impl Future<Output = TcpFixture>) {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
            src_labels,
            ..
        } = fixture.await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());

        let mut opens = src_labels.metric("tcp_open_total").value(1u64);
        let mut closes = src_labels
            .metric("tcp_close_total")
            .label("errno", "")
            .value(1u64);
        opens.assert_in(&metrics).await;

        tcp_client.shutdown().await;
        closes.assert_in(&metrics).await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        opens.set_value(2u64).assert_in(&metrics).await;

        tcp_client.shutdown().await;
        closes.set_value(2u64).assert_in(&metrics).await;
    }

    async fn test_write_bytes_total(fixture: impl Future<Output = TcpFixture>) {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
            src_labels,
            dst_labels,
        } = fixture.await;
        let src = src_labels
            .metric("tcp_write_bytes_total")
            .value(TcpFixture::BYE_MSG.len());

        let dst = dst_labels
            .metric("tcp_write_bytes_total")
            .value(TcpFixture::HELLO_MSG.len());

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        src.assert_in(&metrics).await;
        dst.assert_in(&metrics).await;
    }

    async fn test_read_bytes_total(fixture: impl Future<Output = TcpFixture>) {
        let _trace = trace_init();
        let TcpFixture {
            client,
            metrics,
            proxy: _proxy,
            dst: _dst,
            profile: _profile,
            src_labels,
            dst_labels,
        } = fixture.await;

        let src = src_labels
            .metric("tcp_read_bytes_total")
            .value(TcpFixture::HELLO_MSG.len());

        let dst = dst_labels
            .metric("tcp_read_bytes_total")
            .value(TcpFixture::BYE_MSG.len());

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        tcp_client.shutdown().await;

        src.assert_in(&metrics).await;
        dst.assert_in(&metrics).await;
    }

    async fn test_tcp_open_conns(fixture: impl Future<Output = TcpFixture>) {
        let _trace = trace_init();
        let fixture = fixture.await;
        let client = fixture.client;
        let metrics = fixture.metrics;
        let mut open_conns = fixture.src_labels.metric("tcp_open_connections");

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        open_conns.set_value(1).assert_in(&metrics).await;

        tcp_client.shutdown().await;
        open_conns.set_value(0).assert_in(&metrics).await;

        let tcp_client = client.connect().await;

        tcp_client.write(TcpFixture::HELLO_MSG).await;
        assert_eq!(tcp_client.read().await, TcpFixture::BYE_MSG.as_bytes());
        open_conns.set_value(1).assert_in(&metrics).await;

        tcp_client.shutdown().await;
        open_conns.set_value(0).assert_in(&metrics).await;
    }

    async fn test_http_open_conns(
        fixture: impl Future<Output = Fixture>,
        extra_labels: metrics::Labels,
    ) {
        let _trace = trace_init();
        let Fixture {
            client,
            metrics,
            proxy: _proxy,
            _profile,
            dst_tx: _dst_tx,
            tcp_src_labels,
            ..
        } = fixture.await;

        let mut open_conns = tcp_src_labels
            .and(extra_labels)
            .metric("tcp_open_connections");

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");

        open_conns.set_value(1).assert_in(&metrics).await;
        // Shut down the client to force the connection to close.
        let new_client = client.shutdown().await;
        open_conns.set_value(0).assert_in(&metrics).await;

        // create a new client to force a new connection
        let client = new_client.reconnect();

        info!("client.get(/)");
        assert_eq!(client.get("/").await, "hello");
        open_conns.set_value(1).assert_in(&metrics).await;

        // Shut down the client to force the connection to close.
        client.shutdown().await;
        open_conns.set_value(0).assert_in(&metrics).await;
    }

    #[tokio::test]
    async fn inbound_http_accept() {
        test_http_accept(Fixture::inbound(), metrics::labels()).await;
    }

    #[tokio::test]
    async fn inbound_http_connect() {
        test_http_connect(
            Fixture::inbound(),
            metrics::labels()
                .label("tls", "no_identity")
                .label("no_tls_reason", "loopback"),
        )
        .await;
    }

    #[tokio::test]
    async fn outbound_http_accept() {
        test_http_accept(
            Fixture::outbound(),
            metrics::labels()
                .label("tls", "no_identity")
                .label("no_tls_reason", "loopback"),
        )
        .await;
    }

    #[tokio::test]
    async fn outbound_http_connect() {
        test_http_connect(Fixture::outbound(), metrics::labels()).await;
    }

    #[tokio::test]
    async fn inbound_tcp_connect() {
        test_tcp_connect(TcpFixture::inbound()).await
    }

    #[tokio::test]
    async fn inbound_tcp_accept() {
        test_tcp_accept(TcpFixture::inbound()).await
    }

    #[tokio::test]
    async fn outbound_tcp_connect() {
        test_tcp_connect(TcpFixture::outbound()).await;
    }

    #[cfg_attr(not(feature = "flakey-in-coverage"), ignore)]
    #[tokio::test]
    async fn outbound_tcp_accept() {
        test_tcp_accept(TcpFixture::outbound()).await;
    }

    #[tokio::test]
    async fn inbound_tcp_write_bytes_total() {
        test_write_bytes_total(TcpFixture::inbound()).await
    }

    #[tokio::test]
    async fn inbound_tcp_read_bytes_total() {
        test_read_bytes_total(TcpFixture::inbound()).await
    }

    #[tokio::test]
    async fn outbound_tcp_write_bytes_total() {
        test_write_bytes_total(TcpFixture::outbound()).await
    }

    #[tokio::test]
    async fn outbound_tcp_read_bytes_total() {
        test_read_bytes_total(TcpFixture::outbound()).await
    }

    #[cfg_attr(not(feature = "flakey-in-coverage"), ignore)]
    #[tokio::test]
    async fn outbound_tcp_open_connections() {
        test_tcp_open_conns(TcpFixture::outbound()).await
    }

    #[tokio::test]
    async fn inbound_tcp_open_connections() {
        test_tcp_open_conns(TcpFixture::inbound()).await
    }

    #[tokio::test]
    async fn outbound_http_tcp_open_connections() {
        test_http_open_conns(
            Fixture::outbound(),
            metrics::labels()
                .label("direction", "outbound")
                .label("tls", "no_identity")
                .label("no_tls_reason", "loopback"),
        )
        .await
    }

    #[tokio::test]
    async fn inbound_http_tcp_open_connections() {
        test_http_open_conns(
            Fixture::inbound(),
            metrics::labels().label("direction", "inbound"),
        )
        .await
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
        labels,
        ..
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

    let mut metric = labels
        .metric("response_latency_ms_count")
        .label("status_code", 200)
        .value(1u64);

    for &encoding in encodings {
        assert_eventually_contains!(do_scrape(encoding).await, &metric);
    }

    info!("client.get(/)");
    assert_eq!(client.get("/").await, "hello");

    for &encoding in encodings {
        assert_eventually_contains!(do_scrape(encoding).await, metric.set_value(2u64));
    }
}
