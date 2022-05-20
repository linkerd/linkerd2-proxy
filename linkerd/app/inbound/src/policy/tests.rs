use super::*;
use linkerd_app_core::{proxy::http, Error};
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::collections::HashSet;

#[derive(Clone)]
pub(crate) struct MockSvc;

#[tokio::test(flavor = "current_thread")]
async fn unauthenticated_allowed() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::Unauthenticated,
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            kind: "serverauthorization".into(),
            name: "unauth".into(),
        }],
        kind: "server".into(),
        name: "test".into(),
    };

    let policies = Store::for_test(policy.clone(), None);
    let allowed = policies.get_policy(orig_dst_addr());
    assert_eq!(*allowed.server.borrow(), policy);

    let tls = tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello);
    let permitted = allowed
        .check_authorized(client_addr(), &tls)
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        Permit {
            dst: orig_dst_addr(),
            protocol: policy.protocol,
            labels: AuthzLabels {
                kind: "serverauthorization".into(),
                name: "unauth".into(),
                server: ServerLabel {
                    kind: "server".into(),
                    name: "test".into(),
                }
            }
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_identity() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::TlsAuthenticated {
                suffixes: vec![],
                identities: vec![client_id().to_string()].into_iter().collect(),
            },
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            kind: "serverauthorization".into(),
            name: "tls-auth".into(),
        }],
        kind: "server".into(),
        name: "test".into(),
    };

    let policies = Store::for_test(policy.clone(), None);
    let allowed = policies.get_policy(orig_dst_addr());
    assert_eq!(*allowed.server.borrow(), policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    let permitted = allowed
        .check_authorized(client_addr(), &tls)
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        Permit {
            dst: orig_dst_addr(),
            protocol: policy.protocol,
            labels: AuthzLabels {
                kind: "serverauthorization".into(),
                name: "tls-auth".into(),
                server: ServerLabel {
                    kind: "server".into(),
                    name: "test".into()
                },
            }
        }
    );

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(tls::ClientId(
            "othersa.testns.serviceaccount.identity.linkerd.cluster.local"
                .parse()
                .unwrap(),
        )),
        negotiated_protocol: None,
    });
    allowed
        .check_authorized(client_addr(), &tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_suffix() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::TlsAuthenticated {
                identities: HashSet::default(),
                suffixes: vec![Suffix::from(vec!["cluster".into(), "local".into()])],
            },
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            kind: "serverauthorization".into(),
            name: "tls-auth".into(),
        }],
        kind: "server".into(),
        name: "test".into(),
    };

    let policies = Store::for_test(policy.clone(), None);
    let allowed = policies.get_policy(orig_dst_addr());
    assert_eq!(*allowed.server.borrow(), policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    assert_eq!(
        allowed
            .check_authorized(client_addr(), &tls)
            .expect("unauthenticated connection must be permitted"),
        Permit {
            dst: orig_dst_addr(),
            protocol: policy.protocol,
            labels: AuthzLabels {
                kind: "serverauthorization".into(),
                name: "tls-auth".into(),
                server: ServerLabel {
                    kind: "server".into(),
                    name: "test".into()
                }
            }
        }
    );

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(
            "testsa.testns.serviceaccount.identity.linkerd.cluster.example.com"
                .parse()
                .unwrap(),
        ),
        negotiated_protocol: None,
    });
    allowed
        .check_authorized(client_addr(), &tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn tls_unauthenticated() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::TlsUnauthenticated,
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            kind: "serverauthorization".into(),
            name: "tls-unauth".into(),
        }],
        kind: "server".into(),
        name: "test".into(),
    };

    let policies = Store::for_test(policy.clone(), None);
    let allowed = policies.get_policy(orig_dst_addr());
    assert_eq!(*allowed.server.borrow(), policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: None,
        negotiated_protocol: None,
    });
    assert_eq!(
        allowed
            .check_authorized(client_addr(), &tls)
            .expect("unauthenticated connection must be permitted"),
        Permit {
            dst: orig_dst_addr(),
            protocol: policy.protocol,
            labels: AuthzLabels {
                kind: "serverauthorization".into(),
                name: "tls-unauth".into(),
                server: ServerLabel {
                    kind: "server".into(),
                    name: "test".into(),
                },
            }
        }
    );

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Passthru {
        sni: "othersa.testns.serviceaccount.identity.linkerd.cluster.example.com"
            .parse()
            .unwrap(),
    });
    allowed
        .check_authorized(client_addr(), &tls)
        .expect_err("policy must require a TLS termination identity");
}

fn client_id() -> tls::ClientId {
    "testsa.testns.serviceaccount.identity.linkerd.cluster.local"
        .parse()
        .unwrap()
}

fn client_addr() -> Remote<ClientAddr> {
    Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
}

fn orig_dst_addr() -> OrigDstAddr {
    OrigDstAddr(([192, 0, 2, 2], 1000).into())
}

impl tonic::client::GrpcService<tonic::body::BoxBody> for MockSvc {
    type ResponseBody = linkerd_app_core::control::RspBody;
    type Error = Error;
    type Future = futures::future::Pending<Result<http::Response<Self::ResponseBody>, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Error>> {
        unreachable!()
    }

    fn call(&mut self, _req: http::Request<tonic::body::BoxBody>) -> Self::Future {
        unreachable!()
    }
}

impl Store<MockSvc> {
    pub(crate) fn for_test(
        default: impl Into<DefaultPolicy>,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> Self {
        Self::spawn_fixed(default.into(), std::time::Duration::MAX, ports)
    }
}
