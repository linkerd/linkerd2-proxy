use super::*;
use crate::policy::{Authentication, Authorization, Meta, Protocol, ServerPolicy};
use linkerd_app_core::{svc::Service, Infallible};
use linkerd_http_box::BoxBody;
use linkerd_proxy_server_policy::{LocalRateLimit, RateLimitError};

macro_rules! conn {
    ($client:expr, $dst:expr) => {{
        ConnectionMeta {
            dst: OrigDstAddr(($dst, 8080).into()),
            client: Remote(ClientAddr(($client, 30120).into())),
            tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some("foo.bar.bah".parse().unwrap()),
                negotiated_protocol: None,
            }),
        }
    }};
    () => {{
        conn!([192, 168, 3, 3], [192, 168, 3, 4])
    }};
}

macro_rules! new_svc {
    ($proto:expr, $conn:expr, $rsp:expr, $rl: expr) => {{
        let (policy, tx) = AllowPolicy::for_test(
            $conn.dst,
            ServerPolicy {
                protocol: $proto,
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "Server".into(),
                    name: "testsrv".into(),
                }),
                local_rate_limit: Arc::new($rl),
            },
        );
        let svc = HttpPolicyService {
            target: (),
            policy,
            connection: $conn,
            metrics: HttpAuthzMetrics::default(),
            inner: |(permit, _): (HttpRoutePermit, ())| {
                let f = $rsp;
                svc::mk(move |req: ::http::Request<BoxBody>| {
                    futures::future::ready((f)(permit.clone(), req))
                })
            },
        };
        (svc, tx)
    }};

    ($proto:expr, $conn:expr, $rsp:expr) => {{
        new_svc!($proto, $conn, $rsp, Default::default())
    }};

    ($proto:expr, $rl:expr) => {{
        new_svc!(
            $proto,
            conn!(),
            |permit: HttpRoutePermit, _req: ::http::Request<BoxBody>| {
                let mut rsp = ::http::Response::builder()
                    .body(BoxBody::default())
                    .unwrap();
                rsp.extensions_mut().insert(permit.clone());
                Ok::<_, Infallible>(rsp)
            },
            $rl
        )
    }};

    ($proto:expr) => {{
        new_svc!($proto, Default::default())
    }};
}

#[tokio::test(flavor = "current_thread")]
async fn http_route() {
    use linkerd_proxy_server_policy::http::{r#match::MatchRequest, Policy, Route, Rule};

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let (mut svc, tx) = new_svc!(Protocol::Http1(Arc::new([Route {
        hosts: vec![],
        rules: vec![
            Rule {
                matches: vec![MatchRequest {
                    method: Some(::http::Method::GET),
                    ..MatchRequest::default()
                }],
                policy: Policy {
                    authorizations: Arc::new([Authorization {
                        authentication: Authentication::Unauthenticated,
                        networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                        meta: Arc::new(Meta::Resource {
                            group: "policy.linkerd.io".into(),
                            kind: "AuthorizationPolicy".into(),
                            name: "test".into(),
                        }),
                    }]),
                    filters: vec![],
                    meta: rmeta.clone(),
                },
            },
            Rule {
                matches: vec![MatchRequest {
                    method: Some(::http::Method::POST),
                    ..MatchRequest::default()
                }],
                policy: Policy {
                    authorizations: Arc::new([]),
                    filters: vec![],
                    meta: rmeta.clone(),
                },
            }
        ],
    }])));

    // Test that authorization policies allow requests:
    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect("serves");
    let permit = rsp
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted");
    assert_eq!(permit.labels.route.route, rmeta);

    // And deny requests:
    assert!(svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteUnauthorized>());

    // Test that a route must match the request:
    assert!(svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::DELETE)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteNotFound>());

    // Update all of the policies and then test the same requests to ensure that
    // the requests are handled differently after an update.

    tx.send(ServerPolicy {
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "Server".into(),
            name: "testsrv".into(),
        }),
        protocol: Protocol::Http1(Arc::new([Route {
            hosts: vec![],
            rules: vec![
                Rule {
                    matches: vec![MatchRequest {
                        method: Some(::http::Method::GET),
                        ..MatchRequest::default()
                    }],
                    policy: Policy {
                        authorizations: Arc::new([Authorization {
                            authentication: Authentication::Unauthenticated,
                            networks: vec![std::net::IpAddr::from([172, 2, 2, 2]).into()],
                            meta: Arc::new(Meta::Resource {
                                group: "policy.linkerd.io".into(),
                                kind: "AuthorizationPolicy".into(),
                                name: "other".into(),
                            }),
                        }]),
                        filters: vec![],
                        meta: rmeta.clone(),
                    },
                },
                Rule {
                    matches: vec![MatchRequest {
                        method: Some(::http::Method::DELETE),
                        ..MatchRequest::default()
                    }],
                    policy: Policy {
                        authorizations: Arc::new([Authorization {
                            authentication: Authentication::Unauthenticated,
                            networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                            meta: Arc::new(Meta::Resource {
                                group: "policy.linkerd.io".into(),
                                kind: "AuthorizationPolicy".into(),
                                name: "test".into(),
                            }),
                        }]),
                        filters: vec![],
                        meta: rmeta.clone(),
                    },
                },
            ],
        }])),
        local_rate_limit: Arc::new(Default::default()),
    })
    .expect("must send");

    assert!(svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap(),)
        .await
        .expect_err("fails")
        .is::<HttpRouteUnauthorized>());

    assert!(svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteNotFound>());

    let rsp = svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::DELETE)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect("serves");
    let permit = rsp
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted");
    assert_eq!(permit.labels.route.route, rmeta);
}

#[tokio::test(flavor = "current_thread")]
async fn http_filter_header() {
    use linkerd_proxy_server_policy::http::{
        filter, r#match::MatchRequest, Filter, Policy, Route, Rule,
    };

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let proto = Protocol::Http1(Arc::new([Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRequest {
                method: Some(::http::Method::GET),
                ..MatchRequest::default()
            }],
            policy: Policy {
                authorizations: Arc::new([Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                    meta: Arc::new(Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "AuthorizatoinPolicy".into(),
                        name: "test".into(),
                    }),
                }]),
                filters: vec![Filter::RequestHeaders(filter::ModifyHeader {
                    add: vec![("testkey".parse().unwrap(), "testval".parse().unwrap())],
                    ..filter::ModifyHeader::default()
                })],
                meta: rmeta.clone(),
            },
        }],
    }]));
    let inner = |permit: HttpRoutePermit, req: ::http::Request<BoxBody>| -> Result<_> {
        assert_eq!(req.headers().len(), 1);
        assert_eq!(
            req.headers().get("testkey"),
            Some(&"testval".parse().unwrap())
        );
        let mut rsp = ::http::Response::builder()
            .body(BoxBody::default())
            .unwrap();
        rsp.extensions_mut().insert(permit);
        Ok(rsp)
    };
    let (mut svc, _tx) = new_svc!(proto, conn!(), inner);

    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect("serves");
    let permit = rsp
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted");
    assert_eq!(permit.labels.route.route, rmeta);
}

#[tokio::test(flavor = "current_thread")]
async fn http_filter_inject_failure() {
    use linkerd_proxy_server_policy::http::{
        filter, r#match::MatchRequest, Filter, Policy, Route, Rule,
    };

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let proto = Protocol::Http1(Arc::new([Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRequest {
                method: Some(::http::Method::GET),
                ..MatchRequest::default()
            }],
            policy: Policy {
                authorizations: Arc::new([Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                    meta: Arc::new(Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "AuthorizatoinPolicy".into(),
                        name: "test".into(),
                    }),
                }]),
                filters: vec![Filter::InjectFailure(filter::InjectFailure {
                    distribution: filter::Distribution::from_ratio(1, 1).unwrap(),
                    response: filter::FailureResponse {
                        status: ::http::StatusCode::BAD_REQUEST,
                        message: "oopsie".into(),
                    },
                })],
                meta: rmeta.clone(),
            },
        }],
    }]));
    let inner = |_: HttpRoutePermit,
                 _: ::http::Request<BoxBody>|
     -> Result<::http::Response<BoxBody>> { unreachable!() };
    let (mut svc, _tx) = new_svc!(proto, conn!(), inner);

    let err = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect_err("fails");
    assert_eq!(
        *err.downcast_ref::<HttpRouteInjectedFailure>().unwrap(),
        HttpRouteInjectedFailure {
            status: ::http::StatusCode::BAD_REQUEST,
            message: "oopsie".into(),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_allow() {
    use linkerd_app_core::{Ipv4Net, Ipv6Net};

    let rmeta = Meta::new_default("default");

    // Rate-limit with plenty of room for two consecutive requests
    let rl = LocalRateLimit::new_no_overrides_for_test(Some(10), Some(5));

    let authorizations = Arc::new([Authorization {
        meta: rmeta.clone(),
        networks: vec![Ipv4Net::default().into(), Ipv6Net::default().into()],
        authentication: Authentication::Unauthenticated,
    }]);

    let (mut svc, _tx) = new_svc!(
        Protocol::Http1(Arc::new([http::default(authorizations.clone())])),
        rl
    );

    // First request should be allowed
    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect("serves");
    assert_eq!(rsp.status(), ::http::StatusCode::OK);

    // Second request should be allowed as well
    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect("serves");
    assert_eq!(rsp.status(), ::http::StatusCode::OK);
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_deny() {
    use linkerd_app_core::{Ipv4Net, Ipv6Net};

    let rmeta = Meta::new_default("default");

    // Rate-limit with room for only one request per second
    let rl = LocalRateLimit::new_no_overrides_for_test(Some(10), Some(1));

    let authorizations = Arc::new([Authorization {
        meta: rmeta.clone(),
        networks: vec![Ipv4Net::default().into(), Ipv6Net::default().into()],
        authentication: Authentication::Unauthenticated,
    }]);

    let (mut svc, _tx) = new_svc!(
        Protocol::Http1(Arc::new([http::default(authorizations.clone())])),
        rl
    );

    // First request should be allowed
    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect("serves");
    assert_eq!(rsp.status(), ::http::StatusCode::OK);

    // Second request should be denied
    let rsp = svc
        .call(::http::Request::builder().body(BoxBody::default()).unwrap())
        .await
        .expect_err("should deny");
    let err = rsp
        .downcast_ref::<RateLimitError>()
        .expect("rate limit error");
    match err {
        RateLimitError::PerIdentity(rps) => assert_eq!(rps, &std::num::NonZeroU32::new(1).unwrap()),
        _ => panic!("unexpected error"),
    };
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_route() {
    use linkerd_proxy_server_policy::grpc::{
        r#match::{MatchRoute, MatchRpc},
        Policy, Route, Rule,
    };

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "grpcproute".into(),
        name: "testrt".into(),
    });
    let (mut svc, _tx) = new_svc!(Protocol::Grpc(Arc::new([Route {
        hosts: vec![],
        rules: vec![
            Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo.bar.bah".to_string()),
                        method: Some("baz".to_string()),
                    },
                    ..MatchRoute::default()
                }],
                policy: Policy {
                    authorizations: Arc::new([Authorization {
                        authentication: Authentication::Unauthenticated,
                        networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                        meta: Arc::new(Meta::Resource {
                            group: "policy.linkerd.io".into(),
                            kind: "AuthorizationPolicy".into(),
                            name: "test".into(),
                        }),
                    }]),
                    filters: vec![],
                    meta: rmeta.clone(),
                },
            },
            Rule {
                matches: vec![MatchRoute {
                    rpc: MatchRpc {
                        service: Some("foo.bar.bah".to_string()),
                        method: Some("qux".to_string()),
                    },
                    ..MatchRoute::default()
                }],
                policy: Policy {
                    authorizations: Arc::new([]),
                    filters: vec![],
                    meta: rmeta.clone(),
                },
            }
        ],
    }])));

    let rsp = svc
        .call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/baz")
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect("serves");
    let permit = rsp
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted");
    assert_eq!(permit.labels.route.route, rmeta);

    assert!(svc
        .call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/qux")
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteUnauthorized>());

    assert!(svc
        .call(
            ::http::Request::builder()
                .uri("/boo.bar.bah/bah")
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteNotFound>());
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_filter_header() {
    use linkerd_proxy_server_policy::{
        grpc::{
            r#match::{MatchRoute, MatchRpc},
            Filter, Policy, Route, Rule,
        },
        http,
    };

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let proto = Protocol::Grpc(Arc::new([Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRoute {
                rpc: MatchRpc {
                    service: Some("foo.bar.bah".to_string()),
                    method: Some("baz".to_string()),
                },
                ..MatchRoute::default()
            }],

            policy: Policy {
                authorizations: Arc::new([Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                    meta: Arc::new(Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "AuthorizatoinPolicy".into(),
                        name: "test".into(),
                    }),
                }]),
                filters: vec![Filter::RequestHeaders(http::filter::ModifyHeader {
                    add: vec![("testkey".parse().unwrap(), "testval".parse().unwrap())],
                    ..http::filter::ModifyHeader::default()
                })],
                meta: rmeta.clone(),
            },
        }],
    }]));
    let inner = |permit: HttpRoutePermit, req: ::http::Request<BoxBody>| -> Result<_> {
        assert_eq!(req.headers().len(), 1);
        assert_eq!(
            req.headers().get("testkey"),
            Some(&"testval".parse().unwrap())
        );
        let mut rsp = ::http::Response::builder()
            .body(BoxBody::default())
            .unwrap();
        rsp.extensions_mut().insert(permit);
        Ok(rsp)
    };
    let (mut svc, _tx) = new_svc!(proto, conn!(), inner);

    let rsp = svc
        .call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/baz")
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect("serves");
    let permit = rsp
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted");
    assert_eq!(permit.labels.route.route, rmeta);
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_filter_inject_failure() {
    use linkerd_proxy_server_policy::grpc::{
        filter,
        r#match::{MatchRoute, MatchRpc},
        Filter, Policy, Route, Rule,
    };

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "grpcroute".into(),
        name: "testrt".into(),
    });
    let proto = Protocol::Grpc(Arc::new([Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![MatchRoute {
                rpc: MatchRpc {
                    service: Some("foo.bar.bah".to_string()),
                    method: Some("baz".to_string()),
                },
                ..MatchRoute::default()
            }],

            policy: Policy {
                authorizations: Arc::new([Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![std::net::IpAddr::from([192, 168, 3, 3]).into()],
                    meta: Arc::new(Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "AuthorizatoinPolicy".into(),
                        name: "test".into(),
                    }),
                }]),
                filters: vec![Filter::InjectFailure(filter::InjectFailure {
                    distribution: filter::Distribution::from_ratio(1, 1).unwrap(),
                    response: filter::FailureResponse {
                        code: 4,
                        message: "oopsie".into(),
                    },
                })],
                meta: rmeta.clone(),
            },
        }],
    }]));
    let inner = |_: HttpRoutePermit,
                 _: ::http::Request<BoxBody>|
     -> Result<::http::Response<BoxBody>> { unreachable!() };
    let (mut svc, _tx) = new_svc!(proto, conn!(), inner);

    let err = svc
        .call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/baz")
                .method(::http::Method::POST)
                .body(BoxBody::default())
                .unwrap(),
        )
        .await
        .expect_err("fails");
    assert_eq!(
        *err.downcast_ref::<GrpcRouteInjectedFailure>().unwrap(),
        GrpcRouteInjectedFailure {
            code: 4,
            message: "oopsie".into(),
        }
    );
}
