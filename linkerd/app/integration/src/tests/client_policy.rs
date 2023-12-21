use crate::*;
use linkerd2_proxy_api::{self as api};
use policy::outbound::{self, proxy_protocol};

#[tokio::test]
async fn default_http1_route() {
    let _trace = trace_init();

    const AUTHORITY: &str = "policy.test.svc.cluster.local";

    let srv = server::http1().route("/", "hello h1").run().await;
    let ctrl = controller::new();
    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let dest_tx = ctrl.destination_tx(&dst);
    dest_tx.send_addr(srv.addr);
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);
    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound_default(srv.addr, &dst);

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = client::http1(proxy.outbound, AUTHORITY);

    assert_eq!(client.get("/").await, "hello h1");
    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn empty_http1_route() {
    let _trace = trace_init();

    const AUTHORITY: &str = "policy.test.svc.cluster.local";

    let srv = server::http1().route("/", "hello h1").run().await;
    let ctrl = controller::new();

    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let dst_tx = ctrl.destination_tx(&dst);
    dst_tx.send_addr(srv.addr);
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);
    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr,
            outbound::OutboundPolicy {
                metadata: Some(api::meta::Metadata {
                    kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                }),
                protocol: Some(outbound::ProxyProtocol {
                    kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                        timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                        http1: Some(proxy_protocol::Http1 {
                            routes: vec![outbound::HttpRoute {
                                metadata: Some(httproute_meta("empty")),
                                hosts: Vec::new(),
                                rules: Vec::new(),
                            }],
                            failure_accrual: None,
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![policy::outbound_default_http_route(&dst)],
                            failure_accrual: None,
                        }),
                        opaque: Some(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_default_opaque_route(&dst)],
                        }),
                    })),
                }),
            },
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = client::http1(proxy.outbound, AUTHORITY);
    let rsp = client.request(client.request_builder("/")).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NOT_FOUND);

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn default_http2_route() {
    let _trace = trace_init();

    const AUTHORITY: &str = "policy.test.svc.cluster.local";

    let srv = server::http2().route("/", "hello h2").run().await;
    let ctrl = controller::new();
    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let dest_tx = ctrl.destination_tx(&dst);
    dest_tx.send_addr(srv.addr);
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);
    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound_default(srv.addr, &dst);

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = client::http2(proxy.outbound, AUTHORITY);

    assert_eq!(client.get("/").await, "hello h2");
    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn empty_http2_route() {
    let _trace = trace_init();

    const AUTHORITY: &str = "policy.test.svc.cluster.local";

    let srv = server::http2().route("/", "hello h2").run().await;
    let ctrl = controller::new();

    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let dst_tx = ctrl.destination_tx(&dst);
    dst_tx.send_addr(srv.addr);
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);
    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr,
            outbound::OutboundPolicy {
                metadata: Some(api::meta::Metadata {
                    kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                }),
                protocol: Some(outbound::ProxyProtocol {
                    kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                        timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                        http1: Some(proxy_protocol::Http1 {
                            routes: vec![policy::outbound_default_http_route(&dst)],
                            failure_accrual: None,
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![outbound::HttpRoute {
                                metadata: Some(httproute_meta("empty")),
                                hosts: Vec::new(),
                                rules: Vec::new(),
                            }],
                            failure_accrual: None,
                        }),
                        opaque: Some(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_default_opaque_route(&dst)],
                        }),
                    })),
                }),
            },
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;
    let client = client::http2(proxy.outbound, AUTHORITY);
    let rsp = client.request(client.request_builder("/")).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NOT_FOUND);

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn header_based_routing() {
    let _trace = trace_init();

    const AUTHORITY_WORLD: &str = "world.test.svc.cluster.local";
    const AUTHORITY_SF: &str = "sf.test.svc.cluster.local";
    const AUTHORITY_AUSTIN: &str = "austin.test.svc.cluster.local";
    const HEADER: &str = "x-hello-city";

    let srv = server::http1().route("/", "hello world!").run().await;
    let srv_sf = server::http1()
        .route("/", "hello san francisco!")
        .run()
        .await;
    let srv_austin = server::http1().route("/", "hello austin!").run().await;
    let ctrl = controller::new();

    let dst_world = format!("{AUTHORITY_WORLD}:{}", srv.addr.port());
    let dst_sf = format!("{AUTHORITY_SF}:{}", srv_sf.addr.port());
    let dst_austin = format!("{AUTHORITY_AUSTIN}:{}", srv_sf.addr.port());

    let dst_world_tx = ctrl.destination_tx(&dst_world);
    dst_world_tx.send_addr(srv.addr);
    let dst_sf_tx = ctrl.destination_tx(&dst_sf);
    dst_sf_tx.send_addr(srv_sf.addr);
    let dst_austin_tx = ctrl.destination_tx(&dst_austin);
    dst_austin_tx.send_addr(srv_austin.addr);

    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY_WORLD);

    let mk_header_rule =
        |dst: &str, header: api::http_route::header_match::Value| outbound::http_route::Rule {
            matches: vec![api::http_route::HttpRouteMatch {
                headers: vec![api::http_route::HeaderMatch {
                    name: HEADER.to_string(),
                    value: Some(header),
                }],
                ..Default::default()
            }],
            filters: Vec::new(),
            backends: Some(policy::http_first_available(std::iter::once(
                policy::backend(dst),
            ))),
            request_timeout: None,
        };

    let route = outbound::HttpRoute {
        metadata: Some(httproute_meta("header-based-routing")),
        hosts: Vec::new(),
        rules: vec![
            // generic hello world
            outbound::http_route::Rule {
                matches: Vec::new(),
                filters: Vec::new(),
                backends: Some(policy::http_first_available(std::iter::once(
                    policy::backend(&dst_world),
                ))),
                request_timeout: None,
            },
            // x-hello-city: sf | x-hello-city: san francisco
            mk_header_rule(
                &dst_sf,
                api::http_route::header_match::Value::Regex("sf|san francisco".to_string()),
            ),
            // x-hello-city: austin
            mk_header_rule(
                &dst_austin,
                api::http_route::header_match::Value::Exact("austin".to_string().into_bytes()),
            ),
        ],
    };

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr,
            outbound::OutboundPolicy {
                metadata: Some(api::meta::Metadata {
                    kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                }),
                protocol: Some(outbound::ProxyProtocol {
                    kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                        timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                        http1: Some(proxy_protocol::Http1 {
                            routes: vec![route.clone()],
                            failure_accrual: None,
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![route],
                            failure_accrual: None,
                        }),
                        opaque: Some(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_default_opaque_route(&dst_world)],
                        }),
                    })),
                }),
            },
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY_WORLD);

    let req = move |headers: &[&str]| {
        let mut builder = client.request_builder("/");

        let span = tracing::info_span!("GET /", "{HEADER}: {headers:?}");
        for &value in headers {
            builder = builder.header(HEADER, value);
        }

        let fut = client.request(builder);
        async move {
            tracing::info!("sending request...");
            let res = fut.await.expect("request");
            tracing::info!(?res);
            assert!(
                res.status().is_success(),
                "client.get('/') expects 2xx, got \"{}\"",
                res.status(),
            );
            let stream = res.into_parts().1;
            http_util::body_to_string(stream).await.unwrap()
        }
        .instrument(span)
    };

    // no header, matches default route
    assert_eq!(req(&[]).await, "hello world!");

    // matches SF route
    assert_eq!(req(&["sf"]).await, "hello san francisco!");

    // unknown header value matches default route
    assert_eq!(req(&["paris"]).await, "hello world!");

    // matches austin route
    assert_eq!(req(&["austin"]).await, "hello austin!");

    // also matches sf route regex
    assert_eq!(req(&["san francisco"]).await, "hello san francisco!");

    // multiple headers (matching and non matching)
    assert_eq!(req(&["sf", "paris"]).await, "hello san francisco!");

    // if both rules match, ties are resolved based on ordering.
    // (see: https://gateway-api.sigs.k8s.io/references/spec/#gateway.networking.k8s.io%2fv1beta1.HTTPRouteRule)
    assert_eq!(req(&["sf", "austin"]).await, "hello san francisco!");
    assert_eq!(
        req(&["san francisco", "austin"]).await,
        "hello san francisco!"
    );

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

#[tokio::test]
async fn path_based_routing() {
    let _trace = trace_init();

    const AUTHORITY_WORLD: &str = "world.test.svc.cluster.local";
    const AUTHORITY_SF: &str = "sf.test.svc.cluster.local";
    const AUTHORITY_AUSTIN: &str = "austin.test.svc.cluster.local";
    const AUTHORITY_BYE: &str = "goodbye.test.svc.cluster.local";

    let srv = server::http1()
        .route("/hello", "hello world!")
        .route("/hello/paris", "bonjour paris!")
        .run()
        .await;
    let srv_sf = server::http1()
        .route("/hello/san-francisco", "hello san francisco!")
        .route("/hello/sf", "hello sf!")
        .run()
        .await;
    let srv_austin = server::http1()
        .route("/hello/austin", "hello austin!")
        .run()
        .await;
    let srv_bye = server::http1()
        .route("/goodbye/austin", "goodbye austin!")
        .route("/goodbye/sf", "goodbye san francisco!")
        .route("/goodbye", "goodbye world!")
        .run()
        .await;
    let ctrl = controller::new();

    let dst_world = format!("{AUTHORITY_WORLD}:{}", srv.addr.port());
    let dst_sf = format!("{AUTHORITY_SF}:{}", srv_sf.addr.port());
    let dst_austin = format!("{AUTHORITY_AUSTIN}:{}", srv_austin.addr.port());
    let dst_bye = format!("{AUTHORITY_BYE}:{}", srv_bye.addr.port());

    let dst_world_tx = ctrl.destination_tx(&dst_world);
    dst_world_tx.send_addr(srv.addr);
    let dst_sf_tx = ctrl.destination_tx(&dst_sf);
    dst_sf_tx.send_addr(srv_sf.addr);
    let dst_austin_tx = ctrl.destination_tx(&dst_austin);
    dst_austin_tx.send_addr(srv_austin.addr);
    let dst_bye_tx = ctrl.destination_tx(&dst_bye);
    dst_bye_tx.send_addr(srv_bye.addr);

    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY_WORLD);

    let mk_path_rule =
        |dst: &str, kind: api::http_route::path_match::Kind| outbound::http_route::Rule {
            matches: vec![api::http_route::HttpRouteMatch {
                path: Some(api::http_route::PathMatch { kind: Some(kind) }),
                ..Default::default()
            }],
            filters: Vec::new(),
            backends: Some(policy::http_first_available(std::iter::once(
                policy::backend(dst),
            ))),

            request_timeout: None,
        };

    let route = outbound::HttpRoute {
        metadata: Some(httproute_meta("path-based-routing")),
        hosts: Vec::new(),
        rules: vec![
            // anything
            outbound::http_route::Rule {
                matches: Vec::new(),
                filters: Vec::new(),
                backends: Some(policy::http_first_available(std::iter::once(
                    policy::backend(&dst_world),
                ))),
                request_timeout: None,
            },
            // /goodbye/*
            mk_path_rule(
                &dst_bye,
                api::http_route::path_match::Kind::Prefix("/goodbye".to_string()),
            ),
            // /hello/sf | /hello/san-francisco
            mk_path_rule(
                &dst_sf,
                api::http_route::path_match::Kind::Regex("/hello/(sf|san-francisco)".to_string()),
            ),
            // /hello/austin
            mk_path_rule(
                &dst_austin,
                api::http_route::path_match::Kind::Exact("/hello/austin".to_string()),
            ),
        ],
    };

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr,
            outbound::OutboundPolicy {
                metadata: Some(api::meta::Metadata {
                    kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                }),
                protocol: Some(outbound::ProxyProtocol {
                    kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                        timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                        http1: Some(proxy_protocol::Http1 {
                            routes: vec![route.clone()],
                            failure_accrual: None,
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![route],
                            failure_accrual: None,
                        }),
                        opaque: Some(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_default_opaque_route(&dst_world)],
                        }),
                    })),
                }),
            },
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY_WORLD);

    // no header, matches default route
    assert_eq!(client.get("/hello").await, "hello world!");

    // matches SF route
    assert_eq!(client.get("/hello/sf").await, "hello sf!");

    // matches austin route
    assert_eq!(client.get("/hello/austin").await, "hello austin!");

    // also matches sf route regex
    assert_eq!(
        client.get("/hello/san-francisco").await,
        "hello san francisco!"
    );

    // matches default route
    assert_eq!(client.get("/hello/paris").await, "bonjour paris!");

    // matches goodbye route prefix
    assert_eq!(client.get("/goodbye").await, "goodbye world!");
    assert_eq!(client.get("/goodbye/austin").await, "goodbye austin!");
    assert_eq!(client.get("/goodbye/sf").await, "goodbye san francisco!");

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

//#[tokio::test] -- commenting for now until I wire the test up
async fn outbound_policy_http_forward() {
    let _trace = trace_init();

    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());
    let srv = server::http1().route("/hello", "hello world!").run().await;
    let ctrl = controller::new();
    let srv_addr = srv.addr.clone();

    let profile_tx = ctrl.profile_tx(srv.addr.clone());
    profile_tx.send_err(grpc::Status::invalid_argument(
        "we're pretending this is an external endpoint",
    ));

    let (inbound, _profile_in) = {
        let id_svc = in_identity.service();
        let ctrl = controller::new();
        let mut env = in_identity.env;
        env.put(
            app::env::ENV_INBOUND_DEFAULT_POLICY,
            "all-authenticated".into(),
        );
        let profile_tx = ctrl.profile_tx(srv_addr.clone());
        // Under normal conditions, profile would return an endpoint. Since we
        // cannot test TLS & opaque port remap without a dedicated inbound proxy,
        // assume no target is found instead to assert the address is served by the
        // policy controller.
        profile_tx.send_err(grpc::Status::invalid_argument(
            "we're pretending this is outside of the cluster",
        ));
        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "inbound"))
            .await;
        let proxy = proxy::new()
            .controller(ctrl)
            .identity(id_svc.run().await)
            .inbound(srv_addr.clone())
            .inbound_direct();
        let proxy = proxy.run_with_test_env(env).await;
        (proxy, profile_tx)
    };

    let (outbound, _profile_out) = {
        let ctrl = controller::new();
        let profile_tx = ctrl.profile_tx(srv_addr.clone());
        // Under normal conditions, profile would return an endpoint. Since we
        // cannot test TLS & opaque port remap without a dedicated inbound proxy,
        // assume no target is found instead to assert the address is served by the
        // policy controller.
        profile_tx.send_err(grpc::Status::invalid_argument(
            "we're pretending this is outside of the cluster",
        ));

        let policy = controller::policy()
            .with_inbound_default(policy::all_unauthenticated())
            .outbound(
                srv_addr,
                outbound::OutboundPolicy {
                    metadata: Some(api::meta::Metadata {
                        kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                    }),
                    protocol: Some(outbound::ProxyProtocol {
                        kind: Some(proxy_protocol::Kind::Opaque(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_forward_opaque_route(
                                srv_addr,
                                inbound.inbound.port(),
                                in_svc_acct.to_string(),
                            )],
                        })),
                    }),
                },
            );

        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "outbound"))
            .await;
        let proxy = proxy::new()
            .controller(ctrl)
            .policy(policy.run().await)
            .identity(out_identity.service().run().await)
            .outbound_ip(srv_addr)
            .run_with_test_env(out_identity.env)
            .await;
        (proxy, profile_tx)
    };
    let policy = controller::policy()
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv.addr.clone(),
            outbound::OutboundPolicy {
                metadata: Some(api::meta::Metadata {
                    kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                }),
                protocol: Some(outbound::ProxyProtocol {
                    kind: Some(proxy_protocol::Kind::Opaque(proxy_protocol::Opaque {
                        routes: vec![policy::outbound_forward_opaque_route(
                            srv.addr.clone(),
                            inbound.inbound.port(),
                            in_svc_acct.to_string(),
                        )],
                    })),
                }),
            },
        );
}

/// Assumes discovery is performed on an endpoint that exists in cluster but
/// whose address is outside of the cluster networks CIDR. As a result, it is
/// expected that GetProfile will return a default profile with an endpoint and
/// that the policy API will serve an opaque route with a 'Forward' distributor.
#[tokio::test]
async fn outbound_policy_opaque_forward() {
    let _trace = trace_init();

    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());

    // empty / fwd profile
    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";
    let srv = server::tcp()
        .accept(move |read| {
            assert_eq!(read, msg1.as_bytes());
            msg2
        })
        .run()
        .await;
    let srv_addr = srv.addr;

    let (inbound, _profile_in) = {
        let id_svc = in_identity.service();
        let ctrl = controller::new();
        let mut env = in_identity.env;
        env.put(
            app::env::ENV_INBOUND_DEFAULT_POLICY,
            "all-authenticated".into(),
        );
        let profile_tx = ctrl.profile_tx(srv_addr.clone());
        // Under normal conditions, profile would return an endpoint. Since we
        // cannot test TLS & opaque port remap without a dedicated inbound proxy,
        // assume no target is found instead to assert the address is served by the
        // policy controller.
        profile_tx.send_err(grpc::Status::invalid_argument(
            "we're pretending this is outside of the cluster",
        ));
        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "inbound"))
            .await;
        let proxy = proxy::new()
            .controller(ctrl)
            .identity(id_svc.run().await)
            .inbound(srv)
            .inbound_direct();
        let proxy = proxy.run_with_test_env(env).await;
        (proxy, profile_tx)
    };

    let (outbound, _profile_out) = {
        let ctrl = controller::new();
        let profile_tx = ctrl.profile_tx(srv_addr.clone());
        // Under normal conditions, profile would return an endpoint. Since we
        // cannot test TLS & opaque port remap without a dedicated inbound proxy,
        // assume no target is found instead to assert the address is served by the
        // policy controller.
        profile_tx.send_err(grpc::Status::invalid_argument(
            "we're pretending this is outside of the cluster",
        ));

        let policy = controller::policy()
            .with_inbound_default(policy::all_unauthenticated())
            .outbound(
                srv_addr,
                outbound::OutboundPolicy {
                    metadata: Some(api::meta::Metadata {
                        kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
                    }),
                    protocol: Some(outbound::ProxyProtocol {
                        kind: Some(proxy_protocol::Kind::Opaque(proxy_protocol::Opaque {
                            routes: vec![policy::outbound_forward_opaque_route(
                                srv_addr,
                                inbound.inbound.port(),
                                in_svc_acct.to_string(),
                            )],
                        })),
                    }),
                },
            );

        let ctrl = ctrl
            .run()
            .instrument(tracing::info_span!("ctrl", "outbound"))
            .await;
        let proxy = proxy::new()
            .controller(ctrl)
            .policy(policy.run().await)
            .identity(out_identity.service().run().await)
            .outbound_ip(srv_addr)
            .run_with_test_env(out_identity.env)
            .await;
        (proxy, profile_tx)
    };

    let client = client::tcp(outbound.outbound);

    let tcp_client = client.connect().await;
    tcp_client.write(msg1).await;
    assert_eq!(tcp_client.read().await, msg2.as_bytes());

    tcp_client.shutdown().await;
    outbound.join_servers().await;
}

fn httproute_meta(name: impl ToString) -> api::meta::Metadata {
    api::meta::Metadata {
        kind: Some(api::meta::metadata::Kind::Resource(api::meta::Resource {
            group: "gateway.networking.k8s.io".to_string(),
            kind: "HTTPRoute".to_string(),
            name: name.to_string(),
            namespace: "test".to_string(),
            section: "".to_string(),
            port: 0,
        })),
    }
}
