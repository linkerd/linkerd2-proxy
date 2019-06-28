#![recursion_limit = "128"]
#![deny(warnings)]
#[macro_use]
mod support;
use self::support::*;
use support::tap::TapEventExt;

use std::time::SystemTime;

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

    let expected_auth = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
        client_config,
        server_config,
    } = identity::Identity::new("foo-ns1", expected_auth.to_string());
    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    // let client_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    // let _client_id_svc = identity::Identity::new("bar-ns1", client_id.to_string());

    let srv = server::http1_tls(server_config).route("/", "hello").run();

    let expected_id_svc = controller::identity().certify(move |_| certify_rsp).run();
    let proxy = proxy::new()
        .outbound(srv)
        .identity(expected_id_svc)
        .run_with_test_env(env);

    // let mut tap = tap::tls_client(
    //     proxy.control.unwrap(),
    //     TlsConfig::new(expected_id_svc.client_config, expected_auth),
    // );
    // let events = tap.observe(tap::observe_request());

    let tls_client = client::http1_tls(
        proxy.outbound,
        expected_auth,
        TlsConfig::new(client_config, expected_auth),
    );
    assert_eq!(tls_client.get("/"), "hello");

    // let mut _events = events.wait().take(1);
    // assert!(events.next().expect("next1").is_err());
}
