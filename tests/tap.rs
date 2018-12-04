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
    let _ = env_logger_init();
    let srv = server::http1()
        .route("/", "hello")
        .run();

    let proxy = proxy::new()
        .inbound(srv)
        .run();

    let mut tap = tap::client(proxy.control);
    let events = tap.observe(
        tap::observe_request()
    );

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
    let _ = env_logger_init();
    let srv = server::http2()
        .route_fn("/", |_req| {
            Response::builder()
                .header("grpc-status", "1")
                .body(Default::default())
                .unwrap()
        })
        .run();

    let proxy = proxy::new()
        .inbound(srv)
        .run();

    let mut tap = tap::client(proxy.control);
    let events = tap.observe(
        tap::observe_request()
    );

    let authority = "tap.test.svc.cluster.local";
    let client = client::http2(proxy.inbound, authority);

    let res = client.request(client
        .request_builder("/")
        .header("content-type", "application/grpc+nope")
    );
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["grpc-status"], "1");
    assert_eq!(res.into_body().concat2().wait().unwrap().len(), 0);

    let ev = events.wait().nth(2).expect("nth").expect("stream");

    assert_eq!(ev.response_end_eos_grpc(), 1);
}
