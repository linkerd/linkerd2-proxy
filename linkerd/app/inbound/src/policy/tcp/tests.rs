use super::*;
use crate::policy::*;
use linkerd_app_core::{proxy::http, Error};
use linkerd_server_policy::{authz::Suffix, Authentication, Authorization, Protocol, ServerPolicy};
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone)]
pub(crate) struct MockSvc;

#[tokio::test(flavor = "current_thread")]
async fn unauthenticated_allowed() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque(
            vec![Authorization {
                authentication: Authentication::Unauthenticated,
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "unauth".into(),
                }),
            }]
            .into(),
        ),
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "test".into(),
        }),
    };

    let tls = tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello);
    let permitted = check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        ServerPermit {
            dst: orig_dst_addr(),
            protocol: policy.protocol,
            labels: ServerAuthzLabels {
                authz: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "unauth".into()
                }),
                server: ServerLabel(Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "test".into()
                }))
            },
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_identity() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque(
            vec![Authorization {
                authentication: Authentication::TlsAuthenticated {
                    suffixes: vec![],
                    identities: vec![client_id().to_string()].into_iter().collect(),
                },
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-auth".into(),
                }),
            }]
            .into(),
        ),
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "test".into(),
        }),
    };

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    let permitted = check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        ServerPermit {
            dst: orig_dst_addr(),
            protocol: policy.protocol.clone(),
            labels: ServerAuthzLabels {
                authz: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-auth".into()
                }),
                server: ServerLabel(Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "test".into()
                }))
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
    check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_suffix() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque(
            vec![Authorization {
                authentication: Authentication::TlsAuthenticated {
                    identities: BTreeSet::default(),
                    suffixes: vec![Suffix::from(vec!["cluster".into(), "local".into()])],
                },
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-auth".into(),
                }),
            }]
            .into(),
        ),
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "test".into(),
        }),
    };

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    assert_eq!(
        check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
            .expect("unauthenticated connection must be permitted"),
        ServerPermit {
            dst: orig_dst_addr(),
            protocol: policy.protocol.clone(),
            labels: ServerAuthzLabels {
                authz: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-auth".into()
                }),
                server: ServerLabel(Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "test".into()
                })),
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
    check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn tls_unauthenticated() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque(
            vec![Authorization {
                authentication: Authentication::TlsUnauthenticated,
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                meta: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-unauth".into(),
                }),
            }]
            .into(),
        ),
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "test".into(),
        }),
    };

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: None,
        negotiated_protocol: None,
    });
    assert_eq!(
        check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
            .expect("unauthenticated connection must be permitted"),
        ServerPermit {
            dst: orig_dst_addr(),
            protocol: policy.protocol.clone(),
            labels: ServerAuthzLabels {
                authz: Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "serverauthorization".into(),
                    name: "tls-unauth".into()
                }),
                server: ServerLabel(Arc::new(Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "test".into()
                })),
            }
        }
    );

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Passthru {
        sni: "othersa.testns.serviceaccount.identity.linkerd.cluster.example.com"
            .parse()
            .unwrap(),
    });
    check_authorized(&policy, orig_dst_addr(), client_addr(), &tls)
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
        Self::spawn_fixed(default.into(), std::time::Duration::MAX, ports, None)
    }
}
