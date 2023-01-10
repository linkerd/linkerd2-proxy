use crate::*;
use linkerd2_proxy_api as pb;

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
    let authority = format!("policy.test.svc.cluster.local:{}", srv.addr.port());
    let dest = ctrl.destination_tx(authority.clone());
    dest.send_addr(srv.addr);

    let policy_ctrl = controller::policy();
    let policy = policy_ctrl.outbound_tx(srv.addr);
    policy.send(pb::outbound::Service {
        backend: Some(pb::outbound::Backend {
            backend: Some(pb::outbound::backend::Backend::Dst(
                pb::destination::WeightedDst {
                    authority,
                    weight: 1,
                },
            )),
        }),
        protocol: Some(pb::outbound::ProxyProtocol {
            kind: Some(pb::outbound::proxy_protocol::Kind::Detect(
                pb::outbound::proxy_protocol::Detect {
                    timeout: None,
                    http_routes: vec![pb::outbound::HttpRoute {
                        rules: vec![pb::outbound::http_route::Rule {
                            backends: Vec::new(),
                            matches: vec![pb::http_route::HttpRouteMatch {
                                path: Some(pb::http_route::PathMatch {
                                    kind: Some(pb::http_route::path_match::Kind::Exact(
                                        "/matches".to_string(),
                                    )),
                                }),
                                ..Default::default()
                            }],
                        }],
                        metadata: test_meta(),
                        hosts: Vec::new(),
                    }],
                },
            )),
        }),
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

#[tokio::test]
async fn header_based_routing() {
    let _trace = trace_init();

    let world = server::http1().route("/hello", "world").run().await;

    let ny = server::http1().route("/hello", "new york").run().await;

    let sf = server::http1().route("/hello", "san francisco").run().await;

    let ctrl = controller::new_unordered();
    let _profile = ctrl.profile_tx_default(world.addr, "world.hello.svc.cluster.local");

    let world_auth = format!("world.hello.svc.cluster.local:{}", world.addr.port());
    let world_dest = ctrl.destination_tx(world_auth.clone());
    world_dest.send_addr(world.addr);

    let ny_auth = format!("ny.hello.svc.cluster.local:{}", ny.addr.port());
    let ny_dest = ctrl.destination_tx(ny_auth.clone());
    ny_dest.send_addr(ny.addr);

    let sf_auth = format!("sf.hello.svc.cluster.local:{}", sf.addr.port());
    let sf_dest = ctrl.destination_tx(sf_auth.clone());
    sf_dest.send_addr(sf.addr);

    let policy_ctrl = controller::policy();
    let policy = policy_ctrl.outbound_tx(world.addr);
    policy.send(pb::outbound::Service {
        backend: Some(pb::outbound::Backend {
            backend: Some(pb::outbound::backend::Backend::Dst(
                pb::destination::WeightedDst {
                    authority: world_auth.clone(),
                    weight: 1,
                },
            )),
        }),
        protocol: Some(pb::outbound::ProxyProtocol {
            kind: Some(pb::outbound::proxy_protocol::Kind::Detect(
                pb::outbound::proxy_protocol::Detect {
                    timeout: None,
                    http_routes: vec![pb::outbound::HttpRoute {
                        metadata: test_meta(),
                        hosts: Vec::new(),
                        rules: vec![
                            pb::outbound::http_route::Rule {
                                backends: vec![backend(world_auth, 1)],
                                ..Default::default()
                            },
                            pb::outbound::http_route::Rule {
                                backends: vec![backend(ny_auth, 1)],
                                matches: vec![pb::http_route::HttpRouteMatch {
                                    headers: vec![pb::http_route::HeaderMatch {
                                        name: "hello".to_string(),
                                        value: Some(pb::http_route::header_match::Value::Exact(
                                            "new york".to_string().into_bytes(),
                                        )),
                                    }],
                                    ..Default::default()
                                }],
                            },
                            pb::outbound::http_route::Rule {
                                backends: vec![backend(sf_auth, 1)],
                                matches: vec![pb::http_route::HttpRouteMatch {
                                    headers: vec![pb::http_route::HeaderMatch {
                                        name: "hello".to_string(),
                                        value: Some(pb::http_route::header_match::Value::Exact(
                                            "san francisco".to_string().into_bytes(),
                                        )),
                                    }],
                                    ..Default::default()
                                }],
                            },
                        ],
                    }]
                    .into(),
                },
            )),
        }),
    });

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound(world)
        .policy(policy_ctrl.run().await)
        .run()
        .await;
    let client = client::http1(proxy.outbound, "world.hello.svc.cluster.local");

    assert_eq!(client.get("/hello").await, "world");

    let rsp = client
        .request(
            client
                .request_builder("/hello")
                .header("hello", "san francisco"),
        )
        .await
        .unwrap();
    assert_eq!(
        http_util::body_to_string(rsp.into_body()).await.unwrap(),
        "san francisco"
    );

    let rsp = client
        .request(client.request_builder("/hello").header("hello", "new york"))
        .await
        .unwrap();
    assert_eq!(
        http_util::body_to_string(rsp.into_body()).await.unwrap(),
        "new york"
    );

    let rsp = client
        .request(
            client
                .request_builder("/hello")
                .header("hello", "everyone else"),
        )
        .await
        .unwrap();
    assert_eq!(
        http_util::body_to_string(rsp.into_body()).await.unwrap(),
        "world"
    );

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

fn backend(dst: impl ToString, weight: u32) -> pb::outbound::Backend {
    pb::outbound::Backend {
        backend: Some(pb::outbound::backend::Backend::Dst(
            pb::destination::WeightedDst {
                authority: dst.to_string(),
                weight,
            },
        )),
    }
}

fn test_meta() -> Option<pb::meta::Metadata> {
    Some(pb::meta::Metadata {
        kind: Some(pb::meta::metadata::Kind::Resource(pb::meta::Resource {
            group: "policy.linkerd.io".to_string(),
            kind: "HTTPRoute".to_string(),
            name: "test-route".to_string(),
        })),
    })
}
