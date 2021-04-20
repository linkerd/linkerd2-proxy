use crate::*;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

#[tokio::test]
async fn nonblocking_identity_detection() {
    let _trace = trace_init();

    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", id.to_string());
    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    let id_svc = controller::identity()
        .certify(move |_| certify_rsp)
        .run()
        .await;
    let proxy = proxy::new().identity(id_svc);

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";
    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run()
        .await;

    let proxy = proxy.inbound(srv).run_with_test_env(env).await;
    let client = client::tcp(proxy.inbound);

    // Create an idle connection and then an active connection. Ensure that
    // protocol detection on the idle connection does not block communication on
    // the active connection.
    let _idle = client.connect().await;
    let active = client.connect().await;
    active.write(msg1).await;
    assert_eq!(
        active.read_timeout(Duration::from_secs(2)).await,
        msg2.as_bytes()
    );
    active.shutdown().await; // ensure the client task joins without panicking.
}

macro_rules! generate_tls_accept_test {
    ( client_non_tls: $make_client_non_tls:path, client_tls: $make_client_tls:path) => {
        let _trace = trace_init();
        let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let id_svc = identity::Identity::new("foo-ns1", id.to_string());
        let proxy = proxy::new()
            .identity(id_svc.service().run().await)
            .run_with_test_env(id_svc.env)
            .await;

        tracing::info!("non-tls request to {}", proxy.metrics);
        let non_tls_client = $make_client_non_tls(proxy.metrics, "localhost");
        assert_eventually!(
            non_tls_client
                .request(non_tls_client.request_builder("/ready").method("GET"))
                .await
                .unwrap()
                .status()
                == http::StatusCode::OK
        );

        tracing::info!("tls request to {}", proxy.metrics);
        let tls_client = $make_client_tls(
            proxy.metrics,
            "localhost",
            client::TlsConfig::new(id_svc.client_config, id),
        );
        assert_eventually!(
            tls_client
                .request(tls_client.request_builder("/ready").method("GET"))
                .await
                .unwrap()
                .status()
                == http::StatusCode::OK
        );
    };
}

macro_rules! generate_tls_reject_test {
    ( client: $make_client:path) => {
        let _trace = trace_init();
        let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let identity::Identity {
            env,
            mut certify_rsp,
            client_config,
            ..
        } = identity::Identity::new("foo-ns1", id.to_string());

        certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

        let (_tx, rx) = oneshot::channel();
        let id_svc = controller::identity()
            .certify_async(move |_| rx)
            .run()
            .await;

        let proxy = proxy::new().identity(id_svc).run_with_test_env(env).await;

        let client = $make_client(
            proxy.metrics,
            "localhost",
            client::TlsConfig::new(client_config, id),
        );

        assert!(client
            .request(client.request_builder("/ready").method("GET"))
            .await
            .err()
            .unwrap()
            .is_connect());
    };
}

#[tokio::test]
async fn http1_accepts_tls_after_identity_is_certified() {
    generate_tls_accept_test! {
       client_non_tls:  client::http1,
       client_tls: client::http1_tls
    }
}

#[tokio::test]
async fn http1_rejects_tls_before_identity_is_certified() {
    generate_tls_reject_test! {client: client::http1_tls}
}

#[tokio::test]
async fn http2_accepts_tls_after_identity_is_certified() {
    generate_tls_accept_test! {
       client_non_tls:  client::http2,
       client_tls: client::http2_tls
    }
}

#[tokio::test]
async fn http2_rejects_tls_before_identity_is_certified() {
    generate_tls_reject_test! {client: client::http2_tls}
}

#[tokio::test]
async fn ready() {
    let _trace = trace_init();
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", id.to_string());

    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    let (tx, rx) = oneshot::channel();
    let id_svc = controller::identity()
        .certify_async(move |_| rx)
        .run()
        .await;

    let proxy = proxy::new().identity(id_svc).run_with_test_env(env).await;

    let client = client::http1(proxy.metrics, "localhost");

    let ready = || async {
        client
            .request(client.request_builder("/ready").method("GET"))
            .await
            .unwrap()
    };
    // The proxy's identity has not yet been verified, so it should not be
    // considered ready.
    assert_ne!(ready().await.status(), http::StatusCode::OK);

    // Make the mock identity service respond to the certify request.
    tx.send(certify_rsp)
        .expect("certify rx should not be dropped");

    // Now, the proxy should be ready.
    assert_eventually!(ready().await.status() == http::StatusCode::OK);
}

#[tokio::test]
async fn refresh() {
    let _trace = trace_init();
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        mut env,
        certify_rsp,
        client_config: _,
        server_config: _,
    } = identity::Identity::new("foo-ns1", id.to_string());

    let (expiry_tx, expiry_rx) = oneshot::channel();
    let refreshed = Arc::new(AtomicBool::new(false));

    let rsp1 = certify_rsp.clone();
    let rsp2 = certify_rsp.clone();
    let refreshed2 = refreshed.clone();
    let id_svc = controller::identity()
        .certify(move |_| {
            let mut rsp = rsp1;
            let expiry = SystemTime::now() + Duration::from_millis(20);
            rsp.valid_until = Some(expiry.into());
            expiry_tx.send(expiry).unwrap();
            rsp
        })
        .certify(move |_| {
            let mut rsp = rsp2;
            rsp.valid_until = Some((SystemTime::now() + Duration::from_millis(6000)).into());
            refreshed2.store(true, Ordering::SeqCst);
            rsp
        })
        .run()
        .await;

    // Disable the minimum bound on when to refresh identity for this test.
    env.put(app::env::ENV_IDENTITY_MIN_REFRESH, "0ms".into());
    let _proxy = proxy::new().identity(id_svc).run_with_test_env(env).await;

    let expiry = expiry_rx.await.expect("wait for expiry");
    let how_long = expiry.duration_since(SystemTime::now()).unwrap();

    tokio::time::sleep(how_long).await;

    assert_eventually!(refreshed.load(Ordering::SeqCst));
}

mod require_id_header {
    macro_rules! generate_tests {
        (
            client: $make_client:path,
            connect_error: $connect_error:expr,
            server: $make_server:path,
            tls_server: $make_tls_server:path,
        ) => {
            #[tokio::test]
            #[cfg_attr(not(feature = "flaky_tests"), ignore)]
            async fn orig_dst_client_connects_to_tls_server() {
                let _trace = trace_init();

                let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
                let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

                let app_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
                let app_identity = identity::Identity::new("bar-ns1", app_name.to_string());

                // Make a server that expects `app_identity` identity
                let srv = $make_tls_server(app_identity.server_config)
                    .route("/", "hello")
                    .run()
                    .await;
                let ctrl = controller::new();
                let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
                let dst = ctrl
                    .destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
                dst.send_addr(srv.addr);
                // Make a proxy that has `proxy_identity` identity
                let proxy = proxy::new()
                    .outbound(srv)
                    .controller(ctrl.run().await)
                    .identity(proxy_identity.service().run().await)
                    .run_with_test_env(proxy_identity.env)
                    .await;

                // Make a non-TLS client
                let client = $make_client(proxy.outbound, app_name);

                // Assert a request to `srv` with `l5d-require-id` header
                // succeeds
                assert_eq!(
                    client
                        .request(
                            client
                                .request_builder("/")
                                .header("l5d-require-id", app_name)
                                .method("GET")
                        )
                        .await
                        .unwrap()
                        .status(),
                    http::StatusCode::OK
                );

                // Assert a request to `srv` with incorrect `l5d-require-id`
                // header fails
                //
                // Fails because of reconnect backoff; results in status code
                // 502
                assert_eq!(
                    client
                        .request(
                            client
                                .request_builder("/")
                                .header("l5d-require-id", "hey-its-me")
                                .method("GET")
                        )
                        .await
                        .unwrap()
                        .status(),
                    $connect_error
                );
            }

            #[tokio::test]
            #[cfg_attr(not(feature = "flaky_tests"), ignore)]
            async fn disco_client_connects_to_tls_server() {
                let _trace = trace_init();

                let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
                let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

                let app_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
                let app_identity = identity::Identity::new("bar-ns1", app_name.to_string());

                // Make a server that expects `app_identity` identity
                let srv = $make_tls_server(app_identity.server_config)
                    .route("/", "hello")
                    .run()
                    .await;

                // Add `srv` to discovery service
                let ctrl = controller::new();
                let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
                let dst = ctrl
                    .destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
                dst.send(controller::destination_add_tls(srv.addr, app_name));

                // Make a proxy that has `proxy_identity` identity and no
                // SO_ORIGINAL_DST backup
                let proxy = proxy::new()
                    .controller(ctrl.run().await)
                    .identity(proxy_identity.service().run().await)
                    .run_with_test_env(proxy_identity.env)
                    .await;

                // Make a non-TLS client
                let client = $make_client(proxy.outbound, "disco.test.svc.cluster.local");

                // Assert a request to `srv` through discovery succeeds
                assert_eq!(
                    client
                        .request(client.request_builder("/").method("GET"))
                        .await
                        .unwrap()
                        .status(),
                    http::StatusCode::OK
                );

                // Assert a request to `srv` through discovery with incorrect
                // `l5d-require-id` fails
                assert_eq!(
                    client
                        .request(
                            client
                                .request_builder("/")
                                .header("l5d-require-id", "hey-its-me")
                                .method("GET")
                        )
                        .await
                        .unwrap()
                        .status(),
                    http::StatusCode::FORBIDDEN
                );

                // Assert a request to `srv` through discovery with
                // `l5d-require-id` succeeds
                assert_eq!(
                    client
                        .request(
                            client
                                .request_builder("/")
                                .header("l5d-require-id", app_name)
                                .method("GET")
                        )
                        .await
                        .unwrap()
                        .status(),
                    http::StatusCode::OK
                );
            }

            #[tokio::test]
            #[cfg_attr(not(feature = "flaky_tests"), ignore)]
            async fn orig_dst_client_cannot_connect_to_plaintext_server() {
                let _trace = trace_init();

                let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
                let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

                // Make a non-TLS server
                let srv = $make_server().route("/", "hello").run().await;
                let ctrl = controller::new();
                let _profile = ctrl.profile_tx_default(srv.addr, "disco.test.svc.cluster.local");
                let dst = ctrl
                    .destination_tx(format!("disco.test.svc.cluster.local:{}", srv.addr.port()));
                dst.send_addr(srv.addr);
                // Make a proxy that has `proxy_identity` identity
                let proxy = proxy::new()
                    .outbound(srv)
                    .controller(ctrl.run().await)
                    .identity(proxy_identity.service().run().await)
                    .run_with_test_env(proxy_identity.env)
                    .await;

                // Make a non-TLS client
                let client = $make_client(proxy.outbound, "localhost");

                // Assert a request to `srv` with `l5d-require-id` header fails
                assert_eq!(
                    client
                        .request(
                            client
                                .request_builder("/")
                                .header("l5d-require-id", "hey-its-me")
                                .method("GET")
                        )
                        .await
                        .unwrap()
                        .status(),
                    $connect_error
                );

                // Assert a request to `srv` with no `l5d-require-id` header
                // succeeds
                assert_eq!(
                    client
                        .request(client.request_builder("/").method("GET"))
                        .await
                        .unwrap()
                        .status(),
                    http::StatusCode::OK
                );
            }
        };
    }

    mod http1 {
        use super::super::*;

        generate_tests! {
            client: client::http1,
            connect_error: http::StatusCode::BAD_GATEWAY,
            server: server::http1,
            tls_server: server::http1_tls,
        }
    }

    mod http2 {
        use super::super::*;

        generate_tests! {
            client: client::http2,
            connect_error: http::StatusCode::SERVICE_UNAVAILABLE,
            server: server::http2,
            tls_server: server::http2_tls,
        }
    }
}

#[tokio::test]
async fn identity_header_stripping() {
    let _trace = trace_init();

    let srv = server::http1()
        .route("/ready", "Ready")
        .route_fn("/check-identity", |req| -> Response<Bytes> {
            return match req.headers().get("l5d-identity") {
                Some(_) => Response::builder()
                    .status(http::StatusCode::BAD_REQUEST)
                    .body(Bytes::new())
                    .unwrap(),
                None => Response::builder()
                    .status(http::StatusCode::OK)
                    .body(Bytes::new())
                    .unwrap(),
            };
        })
        .run()
        .await;

    let proxy = proxy::new().inbound(srv).run().await;

    let client = client::http1(proxy.inbound, "identity.test.svc.cluster.local");
    assert_eventually!(
        client
            .request(client.request_builder("/ready").method("GET"))
            .await
            .unwrap()
            .status()
            == http::StatusCode::OK
    );

    assert_eq!(
        client
            .request(
                client
                    .request_builder("/check-identity")
                    .method("GET")
                    .header("l5d-identity", "spoof")
            )
            .await
            .unwrap()
            .status(),
        http::StatusCode::OK
    );
}
