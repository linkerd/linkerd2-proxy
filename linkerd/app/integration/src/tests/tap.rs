use crate::tap::TapEventExt;
use crate::*;
use std::time::SystemTime;

#[tokio::test]
async fn tap_enabled_when_tap_svc_name_set() {
    let _trace = trace_init();

    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let mut identity_env = identity::Identity::new("foo-ns1", identity.to_string());
    identity_env
        .env
        .put("LINKERD2_PROXY_TAP_SVC_NAME", "test-identity".to_string());

    let proxy = proxy::new()
        .identity(identity_env.service().run().await)
        .run_with_test_env(identity_env.env)
        .await;

    assert!(proxy.tap.is_some())
}

#[tokio::test]
async fn tap_disabled_when_identity_disabled() {
    let _trace = trace_init();
    let proxy = proxy::new().disable_identity().run().await;

    assert!(proxy.tap.is_none())
}

#[tokio::test]
async fn tap_disabled_when_tap_svc_name_not_set() {
    let _trace = trace_init();
    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity_env = identity::Identity::new("foo-ns1", identity.to_string());
    let proxy = proxy::new()
        .identity(identity_env.service().run().await)
        .run_with_test_env(identity_env.env)
        .await;
    assert!(proxy.tap.is_none())
}

#[tokio::test]
async fn rejects_incorrect_identity_when_identity_is_expected() {
    let _trace = trace_init();

    let identity = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity_env = identity::Identity::new("foo-ns1", identity.to_string());

    let expected_identity = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let mut expected_identity_env = identity_env.env.clone();
    expected_identity_env.put(app::env::ENV_TAP_SVC_NAME, expected_identity.to_owned());

    let srv = server::http1().route("/", "hello").run().await;

    let in_proxy = proxy::new()
        .inbound(srv)
        .identity(identity_env.service().run().await)
        .run_with_test_env(expected_identity_env)
        .await;

    let tap_proxy = proxy::new()
        .outbound_ip(in_proxy.tap.unwrap())
        .identity(identity_env.service().run().await)
        .run_with_test_env(identity_env.env.clone())
        .await;

    let mut tap = tap::client(tap_proxy.outbound);
    let mut events = tap.observe(tap::observe_request()).await.take(1);

    let client = client::http1(in_proxy.inbound, "localhost");
    assert_eq!(client.get("/").await, "hello");

    assert!(events.next().await.expect("next1").is_err());
}

// FIXME(ver) this test was marked flakey, but now it consistently fails.
#[ignore]
#[tokio::test]
async fn inbound_http1() {
    let _trace = trace_init();

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
    let srv_proxy_identity_svc = controller::identity()
        .certify(move |_| certify_rsp)
        .run()
        .await;

    // Add expected tap service identity
    env.put(
        app::env::ENV_TAP_SVC_NAME,
        client_proxy_authority.to_owned(),
    );

    let srv = server::http1().route("/", "hello").run().await;

    // Run server proxy with server proxy identity service
    let srv_proxy = proxy::new()
        .inbound(srv)
        .identity(srv_proxy_identity_svc)
        .run_with_test_env(env)
        .await;

    // Run client proxy with client proxy identity service.
    let client_proxy = proxy::new()
        .outbound_ip(srv_proxy.tap.unwrap())
        .identity(client_proxy_identity.service().run().await)
        .run_with_test_env(client_proxy_identity.env)
        .await;

    // Wait for the server proxy to become ready
    let client = client::http1(srv_proxy.health, "localhost");
    let ready = || async {
        client
            .request(client.request_builder("/ready").method("GET"))
            .await
            .unwrap()
    };
    assert_eventually!(ready().await.status() == http::StatusCode::OK);

    let mut tap = tap::client_with_auth(client_proxy.outbound, srv_proxy_authority);
    let events = tap
        .observe_with_require_id(tap::observe_request(), srv_proxy_authority)
        .await;

    // Send a request that will be observed via the tap client
    let client = client::http1(srv_proxy.inbound, "localhost");
    assert_eq!(client.get("/").await, "hello");

    let mut events = events.take(3);

    let ev1 = events.next().await.expect("next1").expect("stream1");
    assert!(ev1.is_inbound());
    assert_eq!(ev1.request_init_authority(), "localhost");
    assert_eq!(ev1.request_init_path(), "/");

    let ev2 = events.next().await.expect("next2").expect("stream2");
    assert!(ev2.is_inbound());
    assert_eq!(ev2.response_init_status(), 200);

    let ev3 = events.next().await.expect("next3").expect("stream3");
    assert!(ev3.is_inbound());
    assert_eq!(ev3.response_end_bytes(), 5);
}

// FIXME(ver) this test was marked flakey, but now it consistently fails.
#[ignore]
#[tokio::test]
async fn grpc_headers_end() {
    let _trace = trace_init();

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
    let srv_proxy_identity_svc = controller::identity()
        .certify(move |_| certify_rsp)
        .run()
        .await;

    // Add expected tap service identity
    env.put(
        app::env::ENV_TAP_SVC_NAME,
        client_proxy_authority.to_owned(),
    );

    let srv = server::http2()
        .route_fn("/", |_req| {
            Response::builder()
                .header("grpc-status", "1")
                .body(Default::default())
                .unwrap()
        })
        .run()
        .await;

    // Run server proxy with server proxy identity service
    let srv_proxy = proxy::new()
        .inbound(srv)
        .identity(srv_proxy_identity_svc)
        .run_with_test_env(env)
        .await;

    // Run client proxy with client proxy identity service.
    let client_proxy = proxy::new()
        .outbound_ip(srv_proxy.tap.unwrap())
        .identity(client_proxy_identity.service().run().await)
        .run_with_test_env(client_proxy_identity.env)
        .await;

    // Wait for the server proxy to become ready
    let client = client::http2(srv_proxy.health, "localhost");
    let ready = || async {
        client
            .request(client.request_builder("/ready").method("GET"))
            .await
            .expect("ready")
    };
    assert_eventually!(ready().await.status() == http::StatusCode::OK);

    let mut tap = tap::client_with_auth(client_proxy.outbound, srv_proxy_authority);
    let events = tap
        .observe_with_require_id(tap::observe_request(), srv_proxy_authority)
        .await;

    // Send a request that will be observed via the tap client
    let client = client::http2(srv_proxy.inbound, "localhost");
    let res = client
        .request(
            client
                .request_builder("/")
                .header("content-type", "application/grpc+nope"),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["grpc-status"], "1");
    assert_eq!(
        hyper::body::to_bytes(res.into_body()).await.unwrap().len(),
        0
    );

    let event = events.skip(2).next().await.expect("2nd").expect("stream");
    assert_eq!(event.response_end_eos_grpc(), 1);
}
