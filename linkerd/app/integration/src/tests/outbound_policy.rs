use crate::*;
use linkerd2_proxy_api::{http_route, meta};
use policy::outbound as pb;

#[tokio::test]
async fn returns_404_if_no_http_route_matches() {
    let _trace = trace_init();

    let srv = server::http1()
        .route("/", "hello")
        .route("/matches", "hello")
        .route("/doesnt/match", "hello")
        .run()
        .await;

    let ctrl = controller::new();
    let _profile = ctrl.profile_tx_default(srv.addr, "policy.test.svc.cluster.local");
    let dest = ctrl.destination_tx(format!("policy.test.svc.cluster.local:{}", srv.addr.port()));
    dest.send_addr(srv.addr);

    let policy_ctrl = controller::policy();
    let policy = policy_ctrl.outbound_tx(srv.addr);
    policy.send(pb::Service {
        backends: Vec::new(),
        http_routes: vec![pb::HttpRoute {
            rules: vec![pb::http_route::Rule {
                backends: Vec::new(),
                matches: vec![http_route::HttpRouteMatch {
                    path: Some(http_route::PathMatch {
                        kind: Some(http_route::path_match::Kind::Exact("/matches".to_string())),
                    }),
                    ..Default::default()
                }],
            }],
            metadata: Some(meta::Metadata {
                kind: Some(meta::metadata::Kind::Resource(meta::Resource {
                    group: "policy.linkerd.io".to_string(),
                    kind: "HTTPRoute".to_string(),
                    name: "test-route".to_string(),
                })),
            }),
            hosts: Vec::new(),
        }],
    });

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(srv)
        .policy(policy_ctrl.run().await)
        .run()
        .await;
    let client = client::http1(proxy.outbound, "policy.test.svc.cluster.local");

    let rsp = client.request(client.request_builder("/")).await.unwrap();
    assert_eq!(rsp.status(), 404);

    let rsp = client
        .request(client.request_builder("/matches"))
        .await
        .unwrap();
    assert_eq!(rsp.status(), 200);

    let rsp = client
        .request(client.request_builder("/doesnt/match"))
        .await
        .unwrap();
    assert_eq!(rsp.status(), 404);

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}
