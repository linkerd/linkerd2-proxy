#![recursion_limit = "128"]
#![deny(warnings)]
#[macro_use]
mod support;
use self::support::*;
use support::tap::TapEventExt;

// Flaky: sometimes the admin thread hasn't had a chance to register
// the Taps before the `client.get` is called.
#[test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
fn inbound_http1() {
    let _ = trace_init();
    let srv = server::http1().route("/", "hello").run();

    let proxy = proxy::new().inbound(srv).run();

    let mut tap = tap::client(proxy.control.unwrap());
    let events = tap.observe(tap::observe_request());

    let authority = "tap.test.svc.cluster.local";
    let client = client::http1(proxy.inbound, authority);

    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(3);

    let ev1 = events.next().expect("next1").expect("stream1");
    assert!(ev1.is_inbound());
    assert_eq!(ev1.request_init_authority(), authority);
    assert_eq!(ev1.request_init_path(), "/");

    let ev2 = events.next().expect("next2").expect("stream2");
    assert!(ev2.is_inbound());
    assert_eq!(ev2.response_init_status(), 200);

    let ev3 = events.next().expect("next3").expect("stream3");
    assert!(ev3.is_inbound());
    assert_eq!(ev3.response_end_bytes(), 5);
}

#[test]
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
fn grpc_headers_end() {
    let _ = trace_init();
    let srv = server::http2()
        .route_fn("/", |_req| {
            Response::builder()
                .header("grpc-status", "1")
                .body(Default::default())
                .unwrap()
        })
        .run();

    let proxy = proxy::new().inbound(srv).run();

    let mut tap = tap::client(proxy.control.unwrap());
    let events = tap.observe(tap::observe_request());

    let authority = "tap.test.svc.cluster.local";
    let client = client::http2(proxy.inbound, authority);

    let res = client.request(
        client
            .request_builder("/")
            .header("content-type", "application/grpc+nope"),
    );
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["grpc-status"], "1");
    assert_eq!(res.into_body().concat2().wait().unwrap().len(), 0);

    let ev = events.wait().nth(2).expect("nth").expect("stream");

    assert_eq!(ev.response_end_eos_grpc(), 1);
}

#[test]
fn tap_enabled_by_default() {
    let _ = trace_init();
    let proxy = proxy::new().run();

    assert!(proxy.control.is_some())
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
#[cfg_attr(not(feature = "flaky_tests"), ignore)]
fn tap_accepts_no_identity_when_identity_is_not_expected() {
    let _ = env_logger_init();
    let auth = "tap.test.svc.cluster.local";

    let srv = server::http1().route("/", "hello").run();

    let proxy = proxy::new().inbound(srv).run();
    let mut tap = tap::client(proxy.control.unwrap());
    let events = tap.observe(tap::observe_request());

    let client = client::http1(proxy.inbound, auth);
    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(1);
    let ev1 = events.next().expect("next1").expect("stream1");
    assert!(ev1.is_inbound());
}

#[test]
fn tap_rejects_no_identity_when_identity_is_expected() {
    let auth = "tap.test.svc.cluster.local";
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";

    let mut env = init_env();
    env.put(app::config::ENV_IDENTITY_TAP_IDENTITY, id.to_owned());

    let srv = server::http1().route("/", "hello").run();

    let proxy = proxy::new().inbound(srv).run_with_test_env(env);
    let mut tap = tap::client(proxy.control.unwrap());
    let events = tap.observe(tap::observe_request());

    let client = client::http1(proxy.inbound, auth);
    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(1);
    assert!(events.next().expect("next1").is_err());
}

#[test]
fn tap_rejects_incorrect_identity_when_identity_is_expected() {
    let _ = env_logger_init();

    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity_env = identity::Identity::new("foo-ns1", identity.to_string());

    let expected_identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let mut expected_identity_env = identity_env.env.clone();
    expected_identity_env.put(
        app::config::ENV_IDENTITY_TAP_IDENTITY,
        expected_identity.to_owned(),
    );

    let srv = server::http1().route("/", "hello").run();

    let in_proxy = proxy::new()
        .inbound(srv)
        .identity(identity_env.service().run())
        .run_with_test_env(expected_identity_env);

    let tap_proxy = proxy::new()
        .outbound_ip(in_proxy.control.unwrap())
        .identity(identity_env.service().run())
        .run_with_test_env(identity_env.env.clone());

    let mut tap = tap::client(tap_proxy.outbound_ip.unwrap());
    let events = tap.observe(tap::observe_request());

    let client = client::http1(in_proxy.inbound, "localhost");
    assert_eq!(client.get("/"), "hello");

    let mut events = events.wait().take(1);
    assert!(events.next().expect("next1").is_err());
}
