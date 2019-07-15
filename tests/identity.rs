#![deny(warnings)]
#![recursion_limit = "128"]
#[macro_use]
mod support;
use self::support::*;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

#[test]
fn nonblocking_identity_detection() {
    let _ = trace_init();

    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", id.to_string());
    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    let id_svc = controller::identity().certify(move |_| certify_rsp).run();
    let proxy = proxy::new().identity(id_svc);

    let msg1 = "custom tcp hello";
    let msg2 = "custom tcp bye";
    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run();

    let proxy = proxy.inbound(srv).run_with_test_env(env);
    let client = client::tcp(proxy.inbound);

    // Create an idle connection and then an active connection. Ensure that
    // protocol detection on the idle connection does not block communication on
    // the active connection.
    let _idle = client.connect();
    let active = client.connect();
    active.write(msg1);
    assert_eq!(active.read_timeout(Duration::from_secs(2)), msg2.as_bytes());
}

macro_rules! generate_tls_accept_test {
    ( client_non_tls: $make_client_non_tls:path, client_tls: $make_client_tls:path) => {
        let _ = trace_init();
        let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let id_svc = identity::Identity::new("foo-ns1", id.to_string());
        let proxy = proxy::new()
            .identity(id_svc.service().run())
            .run_with_test_env(id_svc.env);

        let non_tls_client = $make_client_non_tls(proxy.metrics, "localhost");
        assert_eventually!(
            non_tls_client
                .request(non_tls_client.request_builder("/ready").method("GET"))
                .status()
                == http::StatusCode::OK
        );

        let tls_client = $make_client_tls(
            proxy.metrics,
            "localhost",
            client::TlsConfig::new(id_svc.client_config, id),
        );
        assert_eventually!(
            tls_client
                .request(tls_client.request_builder("/ready").method("GET"))
                .status()
                == http::StatusCode::OK
        );
    };
}

macro_rules! generate_tls_reject_test {
    ( client: $make_client:path) => {
        let _ = trace_init();
        let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let identity::Identity {
            env,
            mut certify_rsp,
            client_config,
            ..
        } = identity::Identity::new("foo-ns1", id.to_string());

        certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

        let (_tx, rx) = oneshot::channel();
        let id_svc = controller::identity().certify_async(move |_| rx).run();

        let proxy = proxy::new().identity(id_svc).run_with_test_env(env);

        let client = $make_client(
            proxy.metrics,
            "localhost",
            client::TlsConfig::new(client_config, id),
        );

        assert!(client
            .request_async(client.request_builder("/ready").method("GET"))
            .wait()
            .err()
            .unwrap()
            .is_connect());
    };
}

#[test]
fn http1_accepts_tls_after_identity_is_certified() {
    generate_tls_accept_test! {
       client_non_tls:  client::http1,
       client_tls: client::http1_tls
    }
}

#[test]
fn http1_rejects_tls_before_identity_is_certified() {
    generate_tls_reject_test! {client: client::http1_tls}
}

#[test]
fn http2_accepts_tls_after_identity_is_certified() {
    generate_tls_accept_test! {
       client_non_tls:  client::http2,
       client_tls: client::http2_tls
    }
}

#[test]
fn http2_rejects_tls_before_identity_is_certified() {
    generate_tls_reject_test! {client: client::http2_tls}
}

macro_rules! generate_outbound_tls_accept_not_cert_identity_test {
    (server: $make_server:path, client: $make_client:path) => {
        let _ = trace_init();
        let proxy_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let app_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let proxy_identity = identity::Identity::new("foo-ns1", proxy_id.to_string());

        let (_tx, rx) = oneshot::channel();
        let id_svc = controller::identity().certify_async(move |_| rx).run();

        let app_identity = identity::Identity::new("bar-ns1", app_id.to_string());
        let srv = $make_server(app_identity.server_config)
            .route("/", "hello")
            .run();

        let proxy = proxy::new()
            .outbound(srv)
            .identity(id_svc)
            .run_with_test_env(proxy_identity.env);

        let client = $make_client(
            proxy.outbound,
            app_id,
            client::TlsConfig::new(app_identity.client_config, app_id),
        );

        assert_eventually!(
            client
                .request(client.request_builder("/").method("GET"))
                .status()
                == http::StatusCode::OK
        );
    };
}

#[test]
fn http1_outbound_tls_works_before_identity_is_certified() {
    generate_outbound_tls_accept_not_cert_identity_test! {
        server: server::http1_tls,
        client: client::http1_tls
    }
}

#[test]
fn http2_outbound_tls_works_before_identity_is_certified() {
    generate_outbound_tls_accept_not_cert_identity_test! {
        server: server::http2_tls,
        client: client::http2_tls
    }
}

#[test]
fn ready() {
    let _ = trace_init();
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", id.to_string());

    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    let (tx, rx) = oneshot::channel();
    let id_svc = controller::identity().certify_async(move |_| rx).run();

    let proxy = proxy::new().identity(id_svc).run_with_test_env(env);

    let client = client::http1(proxy.metrics, "localhost");

    let ready = || client.request(client.request_builder("/ready").method("GET"));

    // The proxy's identity has not yet been verified, so it should not be
    // considered ready.
    assert_ne!(ready().status(), http::StatusCode::OK);

    // Make the mock identity service respond to the certify request.
    tx.send(certify_rsp)
        .expect("certify rx should not be dropped");

    // Now, the proxy should be ready.
    assert_eventually!(ready().status() == http::StatusCode::OK);
}

#[test]
fn refresh() {
    let _ = trace_init();
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
        .run();

    // Disable the minimum bound on when to refresh identity for this test.
    env.put(app::config::ENV_IDENTITY_MIN_REFRESH, "0ms".into());
    let _proxy = proxy::new().identity(id_svc).run_with_test_env(env);

    let expiry = expiry_rx.wait().expect("wait for expiry");
    let how_long = expiry.duration_since(SystemTime::now()).unwrap();

    std::thread::sleep(how_long);

    assert_eventually!(refreshed.load(Ordering::SeqCst) == true);
}

#[test]
fn orig_dst_client_can_connect_to_tls_server_with_require_id_header() {
    let _ = trace_init();

    let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

    let app_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let app_identity = identity::Identity::new("bar-ns1", app_name.to_string());

    // Make a server that expects `app_identity` identity
    let srv = server::http2_tls(app_identity.server_config)
        .route("/", "hello")
        .run();

    // Make a proxy that has `proxy_identity` identity
    let proxy = proxy::new()
        .outbound(srv)
        .identity(proxy_identity.service().run())
        .run_with_test_env(proxy_identity.env);

    // Make a non-TLS client
    let client = client::http2(proxy.outbound, app_name);

    // Assert a request to `srv` with incorrect `l5d-require-id` header fails
    //
    // Fails because of reconnect backoff; results in status code 502
    assert_eq!(
        client
            .request(
                client
                    .request_builder("/")
                    .header("l5d-require-id", "hey-its-me")
                    .method("GET")
            )
            .status(),
        http::StatusCode::SERVICE_UNAVAILABLE
    );

    // Assert a request to `srv` with `l5d-require-id` header succeeds
    assert_eq!(
        client
            .request(
                client
                    .request_builder("/")
                    .header("l5d-require-id", app_name)
                    .method("GET")
            )
            .status(),
        http::StatusCode::OK
    );
}

#[test]
fn discovery_client_can_connect_to_tls_server_with_require_id_header() {
    let _ = trace_init();

    let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

    let app_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let app_identity = identity::Identity::new("bar-ns1", app_name.to_string());

    // Make a server that expects `app_identity` identity
    let srv = server::http2_tls(app_identity.server_config)
        .route("/", "hello")
        .run();

    // Add `srv` to discovery service
    let ctrl = controller::new();
    let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
    dst.send(controller::destination_add_tls(srv.addr, app_name));

    // Make a proxy that has `proxy_identity` identity and no SO_ORIGINAL_DST
    // backup
    let proxy = proxy::new()
        .controller(ctrl.run())
        .identity(proxy_identity.service().run())
        .run_with_test_env(proxy_identity.env);

    // Make a non-TLS client
    let client = client::http2(proxy.outbound, "disco.test.svc.cluster.local");

    // Assert a request to `srv` through discovery succeeds
    assert_eq!(
        client
            .request(client.request_builder("/").method("GET"))
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
            .status(),
        http::StatusCode::BAD_GATEWAY
    );

    // Assert a request to `srv` through discovery with `l5d-require-id` succeeds
    assert_eq!(
        client
            .request(
                client
                    .request_builder("/")
                    .header("l5d-require-id", app_name)
                    .method("GET")
            )
            .status(),
        http::StatusCode::OK
    );
}

#[test]
fn orig_dst_client_cannot_connect_to_plaintext_server_with_require_id_header() {
    let _ = trace_init();

    let proxy_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let proxy_identity = identity::Identity::new("foo-ns1", proxy_name.to_string());

    // Make a non-TLS server
    let srv = server::http2().route("/", "hello").run();

    // Make a proxy that has `proxy_identity` identity
    let proxy = proxy::new()
        .outbound(srv)
        .identity(proxy_identity.service().run())
        .run_with_test_env(proxy_identity.env);

    // Make a non-TLS client
    let client = client::http2(proxy.outbound, "localhost");

    // Assert a request to `srv` succeeds
    assert_eq!(
        client
            .request(client.request_builder("/").method("GET"))
            .status(),
        http::StatusCode::OK
    );

    // Assert a request to `srv` with `l5d-require-id` header fails
    //
    // Fails because of reconnect backoff; results in status code 502
    assert_eq!(
        client
            .request(
                client
                    .request_builder("/")
                    .header("l5d-require-id", "hey-its-me")
                    .method("GET")
            )
            .status(),
        http::StatusCode::SERVICE_UNAVAILABLE
    );
}
