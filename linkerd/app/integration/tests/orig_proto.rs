#![deny(warnings, rust_2018_idioms)]
#![type_length_limit = "3361329"]

use linkerd2_app_integration::*;

#[tokio::test]
async fn outbound_http1() {
    let _trace = trace_init();

    // Instead of a second proxy, this mocked h2 server will be the target.
    let srv = server::http2()
        .route_fn("/hint", |req| {
            assert_eq!(req.headers()["l5d-orig-proto"], "HTTP/1.1");
            Response::builder()
                .header("l5d-orig-proto", "HTTP/1.1")
                .body(Default::default())
                .unwrap()
        })
        .run()
        .await;

    let ctrl = controller::new();
    ctrl.profile_tx_default("disco.test.svc.cluster.local");
    let dst = ctrl.destination_tx("disco.test.svc.cluster.local");
    dst.send_h2_hinted(srv.addr);

    let proxy = proxy::new().controller(ctrl.run().await).run().await;

    let client = client::http1(proxy.outbound, "disco.test.svc.cluster.local");

    let res = client
        .request(client.request_builder("/hint"))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.version(), http::Version::HTTP_11);

    // Ensure panics are propagated.
    proxy.join_servers().await;
    srv.join().await;
}

#[tokio::test]
async fn inbound_http1() {
    let _trace = trace_init();

    let srv = server::http1()
        .route_fn("/h1", |req| {
            assert_eq!(req.version(), http::Version::HTTP_11);
            assert!(
                !req.headers().contains_key("l5d-orig-proto"),
                "h1 server shouldn't receive l5d-orig-proto header"
            );
            Response::default()
        })
        .run()
        .await;

    let ctrl = controller::new();
    ctrl.profile_tx_default("disco.test.svc.cluster.local");

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .inbound(srv)
        .run()
        .await;

    // This client will be used as a mocked-other-proxy.
    let client = client::http2(proxy.inbound, "disco.test.svc.cluster.local");

    let res = client
        .request(
            client
                .request_builder("/h1")
                .header("l5d-orig-proto", "HTTP/1.1"),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.version(), http::Version::HTTP_2);

    // Ensure panics are propagated.
    proxy.join_servers().await;
}
