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
                            ..Default::default()
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![policy::outbound_default_http_route(&dst)],
                            ..Default::default()
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
                            ..Default::default()
                        }),
                        http2: Some(proxy_protocol::Http2 {
                            routes: vec![outbound::HttpRoute {
                                metadata: Some(httproute_meta("empty")),
                                hosts: Vec::new(),
                                rules: Vec::new(),
                            }],
                            ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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
        .outbound(srv.addr, http_routes_policy(vec![route], &dst_world));

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
            ..Default::default()
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
                ..Default::default()
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
        .outbound(srv.addr, http_routes_policy(vec![route], &dst_world));

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

#[tokio::test]
async fn route_precedence_ordering() {
    let _trace = trace_init();

    // The body served by the backend a request is *expected* to be routed to.
    const EXPECTED: &str = "expected";
    // The body served by the backend a request must *not* be routed to. A response
    // with this body means the proxy picked a lower-precedence rule.
    const UNEXPECTED: &str = "unexpected";
    const AUTHORITY_EXPECTED: &str = "expected.test.svc.cluster.local";
    const AUTHORITY_UNEXPECTED: &str = "unexpected.test.svc.cluster.local";

    // Every path used below is served by *both* backends, so a request's body
    // tells us which rule the proxy selected -- never whether the backend
    // happens to know the path.
    const PATHS: &[&str] = &["/a/b", "/b/long/path", "/c", "/d", "/e", "/f"];

    let srv_expected = mk_server(PATHS, EXPECTED).await;
    let srv_unexpected = mk_server(PATHS, UNEXPECTED).await;

    let ctrl = controller::new();
    let dst_expected = format!("{AUTHORITY_EXPECTED}:{}", srv_expected.addr.port());
    let dst_unexpected = format!("{AUTHORITY_UNEXPECTED}:{}", srv_unexpected.addr.port());
    let _dst_expected_tx = {
        let tx = ctrl.destination_tx(&dst_expected);
        tx.send_addr(srv_expected.addr);
        tx
    };
    let _dst_unexpected_tx = {
        let tx = ctrl.destination_tx(&dst_unexpected);
        tx.send_addr(srv_unexpected.addr);
        tx
    };
    let _profile_tx = ctrl.profile_tx_default(srv_expected.addr, AUTHORITY_EXPECTED);

    // In every pair below the *lower*-precedence rule is listed first, so that
    // a passing assertion cannot be explained by list order alone.
    let route = outbound::HttpRoute {
        metadata: Some(httproute_meta("precedence")),
        hosts: Vec::new(),
        rules: vec![
            // (1) Exact path beats prefix path -- even though the prefix rule
            //     also matches two headers and the exact rule matches none.
            //     Path outranks every lower-precedence field.
            rule(
                vec![MatchBuilder::prefix("/a")
                    .header("x-1", "1")
                    .header("x-2", "2")
                    .build()],
                &dst_unexpected,
            ),
            rule(vec![MatchBuilder::exact("/a/b").build()], &dst_expected),
            // (2) Between two prefix matches, more matching characters wins.
            rule(vec![MatchBuilder::prefix("/b").build()], &dst_unexpected),
            rule(vec![MatchBuilder::prefix("/b/long").build()], &dst_expected),
            // (3) With paths tied, a rule that matches the method outranks one
            //     that does not consider the method at all.
            rule(vec![MatchBuilder::exact("/c").build()], &dst_unexpected),
            rule(
                vec![MatchBuilder::exact("/c").method_get().build()],
                &dst_expected,
            ),
            // (4) With path and method tied, more header matches wins.
            rule(
                vec![MatchBuilder::exact("/d").header("x-1", "1").build()],
                &dst_unexpected,
            ),
            rule(
                vec![MatchBuilder::exact("/d")
                    .header("x-1", "1")
                    .header("x-2", "2")
                    .build()],
                &dst_expected,
            ),
            // (5) With path, method, and headers tied, more query param
            //     matches wins.
            rule(
                vec![MatchBuilder::exact("/e").query("p", "1").build()],
                &dst_unexpected,
            ),
            rule(
                vec![MatchBuilder::exact("/e")
                    .query("p", "1")
                    .query("q", "2")
                    .build()],
                &dst_expected,
            ),
            // (6) Method outranks *both* headers and query params: a rule
            //     matching only the method beats one matching two headers and
            //     two query params.
            rule(
                vec![MatchBuilder::exact("/f")
                    .header("x-1", "1")
                    .header("x-2", "2")
                    .query("p", "1")
                    .query("q", "2")
                    .build()],
                &dst_unexpected,
            ),
            rule(
                vec![MatchBuilder::exact("/f").method_get().build()],
                &dst_expected,
            ),
        ],
    };

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv_expected.addr,
            http_routes_policy(vec![route], &dst_expected),
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv_expected)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY_EXPECTED);

    let both_headers = &[("x-1", "1"), ("x-2", "2")][..];

    // (1) exact path > prefix path (with more headers).
    assert_eq!(
        get(&client, "/a/b", both_headers).await,
        EXPECTED,
        "an exact path match must outrank a prefix match with more header matches",
    );

    // (2) longer prefix > shorter prefix.
    assert_eq!(
        get(&client, "/b/long/path", &[]).await,
        EXPECTED,
        "the prefix match with more matching characters must win",
    );

    // (3) method match > no method match, with paths tied.
    assert_eq!(
        get(&client, "/c", &[]).await,
        EXPECTED,
        "with paths tied, a matching method must outrank no method match",
    );
    // ...and a request whose method does *not* match falls through to the rule
    // that does not consider the method, which confirms the method match is
    // actually being evaluated rather than ignored.
    assert_eq!(
        send(&client, client.request_builder("/c").method("POST"))
            .await
            .1,
        UNEXPECTED,
        "a method match must exclude requests using a different method",
    );

    // (4) more header matches wins, with path and method tied.
    assert_eq!(
        get(&client, "/d", both_headers).await,
        EXPECTED,
        "with path and method tied, more header matches must win",
    );

    // (5) more query param matches wins, with path, method, and headers tied.
    assert_eq!(
        get(&client, "/e?p=1&q=2", &[]).await,
        EXPECTED,
        "with path, method, and headers tied, more query param matches must win",
    );

    // (6) method outranks headers and query params.
    assert_eq!(
        get(&client, "/f?p=1&q=2", both_headers).await,
        EXPECTED,
        "a method match must outrank more header and query param matches",
    );

    proxy.join_servers().await;
}

#[tokio::test]
async fn tie_between_routes_selects_first() {
    let _trace = trace_init();

    const AUTHORITY_FOO: &str = "foo.test.svc.cluster.local";
    const AUTHORITY_BAR: &str = "bar.test.svc.cluster.local";
    const PATHS: &[&str] = &["/a/b"];

    let srv_foo = mk_server(PATHS, "foo").await;
    let srv_bar = mk_server(PATHS, "bar").await;

    let ctrl = controller::new();
    let dst_foo = format!("{AUTHORITY_FOO}:{}", srv_foo.addr.port());
    let dst_bar = format!("{AUTHORITY_BAR}:{}", srv_bar.addr.port());
    let _dst_foo_tx = {
        let tx = ctrl.destination_tx(&dst_foo);
        tx.send_addr(srv_foo.addr);
        tx
    };
    let _dst_bar_tx = {
        let tx = ctrl.destination_tx(&dst_bar);
        tx.send_addr(srv_bar.addr);
        tx
    };
    let _profile_tx = ctrl.profile_tx_default(srv_foo.addr, AUTHORITY_FOO);

    // The first-listed route is named so that it sorts *last*, so that list
    // order and name order predict different winners.
    let routes = vec![
        outbound::HttpRoute {
            metadata: Some(httproute_meta("z")),
            hosts: Vec::new(),
            rules: vec![rule(vec![MatchBuilder::prefix("/a").build()], &dst_foo)],
        },
        outbound::HttpRoute {
            metadata: Some(httproute_meta("a")),
            hosts: Vec::new(),
            rules: vec![rule(vec![MatchBuilder::prefix("/a").build()], &dst_bar)],
        },
    ];

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(srv_foo.addr, http_routes_policy(routes, &dst_foo));

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv_foo)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY_FOO);

    assert_eq!(get(&client, "/a/b", &[]).await, "foo");

    proxy.join_servers().await;
}

#[tokio::test]
async fn tie_broken_by_first_matching_rule() {
    let _trace = trace_init();

    // The body served by the backend a request is *expected* to be routed to.
    const EXPECTED: &str = "expected";
    // The body served by the backend a request must *not* be routed to. A response
    // with this body means the proxy picked a lower-precedence rule.
    const UNEXPECTED: &str = "unexpected";
    const AUTHORITY_EXPECTED: &str = "expected.test.svc.cluster.local";
    const AUTHORITY_UNEXPECTED: &str = "unexpected.test.svc.cluster.local";
    const PATHS: &[&str] = &["/tie", "/implicit"];

    let srv_expected = mk_server(PATHS, EXPECTED).await;
    let srv_unexpected = mk_server(PATHS, UNEXPECTED).await;

    let ctrl = controller::new();
    let dst_expected = format!("{AUTHORITY_EXPECTED}:{}", srv_expected.addr.port());
    let dst_unexpected = format!("{AUTHORITY_UNEXPECTED}:{}", srv_unexpected.addr.port());
    let _dst_expected_tx = {
        let tx = ctrl.destination_tx(&dst_expected);
        tx.send_addr(srv_expected.addr);
        tx
    };
    let _dst_unexpected_tx = {
        let tx = ctrl.destination_tx(&dst_unexpected);
        tx.send_addr(srv_unexpected.addr);
        tx
    };
    let _profile_tx = ctrl.profile_tx_default(srv_expected.addr, AUTHORITY_EXPECTED);

    let route = outbound::HttpRoute {
        metadata: Some(httproute_meta("first-matching-rule")),
        hosts: Vec::new(),
        rules: vec![
            // Three rules with byte-identical matches: every specificity
            // criterion ties, so the first must win.
            rule(
                vec![MatchBuilder::exact("/tie").header("x-1", "1").build()],
                &dst_expected,
            ),
            rule(
                vec![MatchBuilder::exact("/tie").header("x-1", "1").build()],
                &dst_unexpected,
            ),
            rule(
                vec![MatchBuilder::exact("/tie").header("x-1", "1").build()],
                &dst_unexpected,
            ),
            // Rules with *no* matches carry the spec's implicit "prefix /"
            // match, so they tie with each other too.
            rule(Vec::new(), &dst_expected),
            rule(Vec::new(), &dst_unexpected),
        ],
    };

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(
            srv_expected.addr,
            http_routes_policy(vec![route], &dst_expected),
        );

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv_expected)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY_EXPECTED);

    assert_eq!(
        get(&client, "/tie", &[("x-1", "1")]).await,
        EXPECTED,
        "with all criteria tied, the first matching rule in list order must win",
    );
    assert_eq!(
        get(&client, "/implicit", &[]).await,
        EXPECTED,
        "rules with no matches tie on the implicit \"prefix /\" match, so the \
         first must win",
    );

    proxy.join_servers().await;
}

// A request that matches no rule in any route is not routable, and must not be
// silently sent to some arbitrary backend.
#[tokio::test]
async fn no_matching_route_returns_404() {
    let _trace = trace_init();

    // The body served by the backend a request is *expected* to be routed to.
    const EXPECTED: &str = "expected";
    const AUTHORITY: &str = "policy.test.svc.cluster.local";
    const PATHS: &[&str] = &["/matches", "/does-not-match"];

    let srv = mk_server(PATHS, EXPECTED).await;

    let ctrl = controller::new();
    let dst = format!("{AUTHORITY}:{}", srv.addr.port());
    let _dst_tx = {
        let tx = ctrl.destination_tx(&dst);
        tx.send_addr(srv.addr);
        tx
    };
    let _profile_tx = ctrl.profile_tx_default(srv.addr, AUTHORITY);

    // The route has rules, but none of them match `/does-not-match`. The
    // backend *does* serve that path, so a 2xx would mean the proxy routed a
    // request it should have rejected.
    let route = outbound::HttpRoute {
        metadata: Some(httproute_meta("no-match")),
        hosts: Vec::new(),
        rules: vec![
            rule(vec![MatchBuilder::exact("/matches").build()], &dst),
            rule(
                vec![MatchBuilder::exact("/does-not-match")
                    .header("x-required", "1")
                    .build()],
                &dst,
            ),
        ],
    };

    let policy = controller::policy()
        // stop the admin server from entering an infinite retry loop
        .with_inbound_default(policy::all_unauthenticated())
        .outbound(srv.addr, http_routes_policy(vec![route], &dst));

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .policy(policy.run().await)
        .outbound(srv)
        .run()
        .await;

    let client = client::http1(proxy.outbound, AUTHORITY);

    // A path no rule matches at all.
    let (status, _) = send(&client, client.request_builder("/does-not-match")).await;
    assert_eq!(
        status,
        http::StatusCode::NOT_FOUND,
        "a request matching no rule must be rejected with 404",
    );

    // A path whose rule matches, but whose required header is absent.
    let (status, _) = send(
        &client,
        client.request_builder("/does-not-match").header("x-1", "1"),
    )
    .await;
    assert_eq!(
        status,
        http::StatusCode::NOT_FOUND,
        "a request that fails a rule's header match must be rejected with 404",
    );

    // The rule that *does* match still works, proving the 404s above are caused
    // by matching and not by a broken fixture.
    let (status, body) = send(&client, client.request_builder("/matches")).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(body, EXPECTED);

    proxy.join_servers().await;
}

// Builds a server that serves `body` at each of `paths`.
async fn mk_server(paths: &[&str], body: &str) -> server::Listening {
    let mut srv = server::http1();
    for path in paths {
        srv = srv.route(path, body);
    }
    srv.run().await
}

// Sends a request, asserting it succeeded, and returns the response body.
async fn get(client: &client::Client, path: &str, headers: &[(&str, &str)]) -> String {
    let mut builder = client.request_builder(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let (status, body) = send(client, builder).await;
    assert!(
        status.is_success(),
        "GET {path} expected 2xx, got {status} (body: {body:?})",
    );
    body
}

// Sends a request and returns its status and body, without asserting on either.
async fn send(
    client: &client::Client,
    builder: http::request::Builder,
) -> (http::StatusCode, String) {
    let rsp = client.request(builder).await.expect("request");
    let status = rsp.status();
    let body = http_util::body_to_string(rsp.into_parts().1)
        .await
        .expect("body");
    (status, body)
}

// Builds a rule routing `matches` to a single backend at `dst`.
fn rule(matches: Vec<api::http_route::HttpRouteMatch>, dst: &str) -> outbound::http_route::Rule {
    outbound::http_route::Rule {
        matches,
        filters: Vec::new(),
        backends: Some(policy::http_first_available(std::iter::once(
            policy::backend(dst),
        ))),
        ..Default::default()
    }
}

// Builds an outbound policy serving `routes` over both HTTP/1 and HTTP/2.
fn http_routes_policy(
    routes: Vec<outbound::HttpRoute>,
    opaque_dst: &str,
) -> outbound::OutboundPolicy {
    outbound::OutboundPolicy {
        metadata: Some(api::meta::Metadata {
            kind: Some(api::meta::metadata::Kind::Default("test".to_string())),
        }),
        protocol: Some(outbound::ProxyProtocol {
            kind: Some(proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                timeout: Some(Duration::from_secs(10).try_into().unwrap()),
                http1: Some(proxy_protocol::Http1 {
                    routes: routes.clone(),
                    ..Default::default()
                }),
                http2: Some(proxy_protocol::Http2 {
                    routes,
                    ..Default::default()
                }),
                opaque: Some(proxy_protocol::Opaque {
                    routes: vec![policy::outbound_default_opaque_route(opaque_dst)],
                }),
            })),
        }),
    }
}

struct MatchBuilder(api::http_route::HttpRouteMatch);

impl MatchBuilder {
    fn path(kind: api::http_route::path_match::Kind) -> Self {
        Self(api::http_route::HttpRouteMatch {
            path: Some(api::http_route::PathMatch { kind: Some(kind) }),
            ..Default::default()
        })
    }

    fn exact(path: &str) -> Self {
        Self::path(api::http_route::path_match::Kind::Exact(path.to_string()))
    }

    fn prefix(path: &str) -> Self {
        Self::path(api::http_route::path_match::Kind::Prefix(path.to_string()))
    }

    fn method_get(mut self) -> Self {
        self.0.method = Some(api::http_types::HttpMethod {
            r#type: Some(api::http_types::http_method::Type::Registered(
                api::http_types::http_method::Registered::Get as i32,
            )),
        });
        self
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.0.headers.push(api::http_route::HeaderMatch {
            name: name.to_string(),
            value: Some(api::http_route::header_match::Value::Exact(
                value.to_string().into_bytes(),
            )),
        });
        self
    }

    fn query(mut self, name: &str, value: &str) -> Self {
        self.0.query_params.push(api::http_route::QueryParamMatch {
            name: name.to_string(),
            value: Some(api::http_route::query_param_match::Value::Exact(
                value.to_string(),
            )),
        });
        self
    }

    fn build(self) -> api::http_route::HttpRouteMatch {
        self.0
    }
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
