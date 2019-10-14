#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1110183"]

mod support;

use self::support::*;
use crate::support::tap::TapEventExt;
use std::time::SystemTime;

#[test]
fn tap_enabled_when_identity_enabled() {
    let _ = trace_init();

    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity_env = identity::Identity::new("foo-ns1", identity.to_string());

    let proxy = proxy::new()
        .identity(identity_env.service().run())
        .run_with_test_env(identity_env.env);

    assert!(proxy.control.is_some())
}

#[test]
fn tap_disabled_when_identity_disabled() {
    let _ = trace_init();
    let proxy = proxy::new().disable_identity().run();

    assert!(proxy.control.is_none())
}

#[test]
fn can_disable_tap() {
    let _ = trace_init();
    let mut env = app::config::TestEnv::new();
    env.put(app::config::ENV_TAP_DISABLED, "true".to_owned());

    let proxy = proxy::new().run_with_test_env(env);

    assert!(proxy.control.is_none())
}

#[test]
fn rejects_incorrect_identity_when_identity_is_expected() {
    let _ = trace_init();

    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity_env = identity::Identity::new("foo-ns1", identity.to_string());

    let expected_identity = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let mut expected_identity_env = identity_env.env.clone();
    expected_identity_env.put(app::config::ENV_TAP_SVC_NAME, expected_identity.to_owned());

    let srv = server::http1().route("/", "hello").run();

    let in_proxy = proxy::new()
        .inbound(srv)
        .identity(identity_env.service().run())
        .run_with_test_env(expected_identity_env);

    let tap_proxy = proxy::new()
        .outbound_ip(in_proxy.control.unwrap())
        .identity(identity_env.service().run())
        .run_with_test_env(identity_env.env.clone());

    let mut tap = tap::client(tap_proxy.outbound);
    let events = tap.observe(tap::observe_request());

    let client = client::http1(in_proxy.inbound, "localhost");
    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(1);
    assert!(events.next().expect("next1").is_err());
}

// Flaky: sometimes the admin thread hasn't had a chance to register
// the Taps before the `client.get` is called.
#[test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
fn inbound_http1() {
    let _ = trace_init();

    // Setup server proxy authority and identity
    let srv_proxy_authority = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        mut env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", srv_proxy_authority.to_string());

    // Setup client proxy authority and identity
    let client_proxy_authority = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let client_proxy_identity =
        identity::Identity::new("bar-ns1", client_proxy_authority.to_string());

    // Certify server proxy identity
    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());
    let srv_proxy_identity_svc = controller::identity().certify(move |_| certify_rsp).run();

    // Add expected tap service identity
    env.put(
        app::config::ENV_TAP_SVC_NAME,
        client_proxy_authority.to_owned(),
    );

    let srv = server::http1().route("/", "hello").run();

    // Run server proxy with server proxy identity service
    let srv_proxy = proxy::new()
        .inbound(srv)
        .identity(srv_proxy_identity_svc)
        .run_with_test_env(env);

    // Run client proxy with client proxy identity service.
    let client_proxy = proxy::new()
        .outbound_ip(srv_proxy.control.unwrap())
        .identity(client_proxy_identity.service().run())
        .run_with_test_env(client_proxy_identity.env);

    // Wait for the server proxy to become ready
    let client = client::http1(srv_proxy.metrics, "localhost");
    let ready = || client.request(client.request_builder("/ready").method("GET"));
    assert_eventually!(ready().status() == http::StatusCode::OK);

    let mut tap = tap::client_with_auth(client_proxy.outbound, srv_proxy_authority);
    let events = tap.observe_with_require_id(tap::observe_request(), srv_proxy_authority);

    // Send a request that will be observed via the tap client
    let client = client::http1(srv_proxy.inbound, "localhost");
    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(3);

    let ev1 = events.next().expect("next1").expect("stream1");
    assert!(ev1.is_inbound());
    assert_eq!(ev1.request_init_authority(), "localhost");
    assert_eq!(ev1.request_init_path(), "/");

    let ev2 = events.next().expect("next2").expect("stream2");
    assert!(ev2.is_inbound());
    assert_eq!(ev2.response_init_status(), 200);

    let ev3 = events.next().expect("next3").expect("stream3");
    assert!(ev3.is_inbound());
    assert_eq!(ev3.response_end_bytes(), 5);
}

#[test]
fn grpc_headers_end() {
    let _ = trace_init();

    // Setup server proxy authority and identity
    let srv_proxy_authority = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        mut env,
        mut certify_rsp,
        ..
    } = identity::Identity::new("foo-ns1", srv_proxy_authority.to_string());

    // Setup client proxy authority and identity
    let client_proxy_authority = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let client_proxy_identity =
        identity::Identity::new("bar-ns1", client_proxy_authority.to_string());

    // Certify server proxy identity
    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());
    let srv_proxy_identity_svc = controller::identity().certify(move |_| certify_rsp).run();

    // Add expected tap service identity
    env.put(
        app::config::ENV_TAP_SVC_NAME,
        client_proxy_authority.to_owned(),
    );

    let srv = server::http2()
        .route_fn("/", |_req| {
            Response::builder()
                .header("grpc-status", "1")
                .body(Default::default())
                .unwrap()
        })
        .run();

    // Run server proxy with server proxy identity service
    let srv_proxy = proxy::new()
        .inbound(srv)
        .identity(srv_proxy_identity_svc)
        .run_with_test_env(env);

    // Run client proxy with client proxy identity service.
    let client_proxy = proxy::new()
        .outbound_ip(srv_proxy.control.unwrap())
        .identity(client_proxy_identity.service().run())
        .run_with_test_env(client_proxy_identity.env);

    // Wait for the server proxy to become ready
    let client = client::http2(srv_proxy.metrics, "localhost");
    let ready = || client.request(client.request_builder("/ready").method("GET"));
    assert_eventually!(ready().status() == http::StatusCode::OK);

    let mut tap = tap::client_with_auth(client_proxy.outbound, srv_proxy_authority);
    let events = tap.observe_with_require_id(tap::observe_request(), srv_proxy_authority);

    // Send a request that will be observed via the tap client
    let client = client::http2(srv_proxy.inbound, "localhost");
    let res = client.request(
        client
            .request_builder("/")
            .header("content-type", "application/grpc+nope"),
    );
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["grpc-status"], "1");
    assert_eq!(res.into_body().concat2().wait().unwrap().len(), 0);

    let event = events.wait().nth(2).expect("nth").expect("stream");

    assert_eq!(event.response_end_eos_grpc(), 1);
}
