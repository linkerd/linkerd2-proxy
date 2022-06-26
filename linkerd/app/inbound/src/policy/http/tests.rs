use super::*;
use crate::policy::{Authentication, Authorization, Meta, Protocol, ServerPolicy};
use linkerd_app_core::{svc::Service, Infallible};
use std::sync::Arc;

macro_rules! conn {
    () => {{
        ConnectionMeta {
            dst: OrigDstAddr(([192, 168, 3, 4], 8080).into()),
            client: Remote(ClientAddr(([192, 168, 3, 3], 30120).into())),
            tls: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some("foo.bar.bah".parse().unwrap()),
                negotiated_protocol: None,
            }),
        }
    }};
}

macro_rules! new_svc {
    ($proto:expr, $conn:expr, $rsp:expr) => {{
        let (policy, tx) = AllowPolicy::for_test(
            $conn.dst,
            ServerPolicy {
                protocol: $proto,
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "testsrv".into(),
                }),
            },
        );
        let svc = HttpPolicyService {
            target: (),
            policy,
            connection: $conn,
            metrics: HttpAuthzMetrics::default(),
            inner: |(permit, _): (HttpRoutePermit, ())| {
                svc::mk(move |req: ::http::Request<hyper::Body>| {
                    futures::future::ready($rsp(permit.clone(), req))
                })
            },
        };
        (svc, tx)
    }};

    ($proto:expr) => {{
        new_svc!(
            $proto,
            conn!(),
            |permit: HttpRoutePermit, _req: ::http::Request<hyper::Body>| {
                let mut rsp = ::http::Response::builder()
                    .body(hyper::Body::default())
                    .unwrap();
                rsp.extensions_mut().insert(permit.clone());
                Ok::<_, Infallible>(rsp)
            }
        )
    }};
}

#[tokio::test(flavor = "current_thread")]
async fn http_route() {
    use linkerd_server_policy::http::{r#match::MatchRequest, Policy, Route, Rule};

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let (mut svc, _tx) = new_svc!(Protocol::Http1(Arc::new([Route {
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
                            kind: "server".into(),
                            name: "testsaz".into(),
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

    assert_eq!(
        svc.call(
            ::http::Request::builder()
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect("serves")
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted")
        .labels
        .route
        .route,
        rmeta
    );

    assert!(svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::POST)
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteUnauthorized>());

    assert!(svc
        .call(
            ::http::Request::builder()
                .method(::http::Method::DELETE)
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteNotFound>());
}

#[tokio::test(flavor = "current_thread")]
async fn http_filters_headers() {
    use linkerd_server_policy::http::{filter, r#match::MatchRequest, Filter, Policy, Route, Rule};

    let rmeta = Arc::new(Meta::Resource {
        group: "gateway.networking.k8s.io".into(),
        kind: "httproute".into(),
        name: "testrt".into(),
    });
    let (mut svc, _tx) = new_svc!(Protocol::Http1(Arc::new([Route {
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
                        kind: "server".into(),
                        name: "testsaz".into(),
                    }),
                }]),
                filters: vec![Filter::RequestHeaders(filter::ModifyRequestHeader {
                    add: vec![("testkey".parse().unwrap(), "testval".parse().unwrap())],
                    ..filter::ModifyRequestHeader::default()
                })],
                meta: rmeta.clone(),
            },
        },],
    }])));

    let req = svc
        .call(
            ::http::Request::builder()
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect("serves");
    assert_eq!(req.headers().iter().collect::<Vec<_>>(), vec![]);
    assert_eq!(
        req.extensions()
            .get::<HttpRoutePermit>()
            .expect("permitted")
            .labels
            .route
            .route,
        rmeta
    );
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_route() {
    use linkerd_server_policy::grpc::{
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
                            kind: "server".into(),
                            name: "testsaz".into(),
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

    assert_eq!(
        svc.call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/baz")
                .method(::http::Method::POST)
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect("serves")
        .extensions()
        .get::<HttpRoutePermit>()
        .expect("permitted")
        .labels
        .route
        .route,
        rmeta
    );

    assert!(svc
        .call(
            ::http::Request::builder()
                .uri("/foo.bar.bah/qux")
                .method(::http::Method::POST)
                .body(hyper::Body::default())
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
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect_err("fails")
        .is::<HttpRouteNotFound>());
}
