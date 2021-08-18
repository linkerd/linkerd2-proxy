use super::*;
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::collections::HashSet;

#[tokio::test(flavor = "current_thread")]
async fn unauthenticated_allowed() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::Unauthenticated,
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            labels: vec![("authz".to_string(), "unauth".to_string())]
                .into_iter()
                .collect(),
        }],
        labels: vec![("server".to_string(), "test".to_string())]
            .into_iter()
            .collect(),
    };

    let policies = Store::fixed(policy.clone(), None);
    let allowed = policies
        .check_policy(orig_dst_addr())
        .expect("port must be known");
    assert_eq!(*allowed.server, policy);

    let tls = tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello);
    let permitted = allowed
        .check_authorized(client_addr(), tls.clone())
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        Permitted {
            tls,
            protocol: policy.protocol,
            labels: vec![
                ("authz".to_string(), "unauth".to_string()),
                ("server".to_string(), "test".to_string())
            ]
            .into_iter()
            .collect()
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
            labels: vec![("authz".to_string(), "tls-auth".to_string())]
                .into_iter()
                .collect(),
        }],
        labels: vec![("server".to_string(), "test".to_string())]
            .into_iter()
            .collect(),
    };

    let policies = Store::fixed(policy.clone(), None);
    let allowed = policies
        .check_policy(orig_dst_addr())
        .expect("port must be known");
    assert_eq!(*allowed.server, policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    let permitted = allowed
        .check_authorized(client_addr(), tls.clone())
        .expect("unauthenticated connection must be permitted");
    assert_eq!(
        permitted,
        Permitted {
            tls,
            protocol: policy.protocol,
            labels: vec![
                ("authz".to_string(), "tls-auth".to_string()),
                ("server".to_string(), "test".to_string())
            ]
            .into_iter()
            .collect()
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
        .check_authorized(client_addr(), tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn authenticated_suffix() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::TlsAuthenticated {
                identities: HashSet::default(),
                suffixes: vec![Suffix::from(vec![
                    "cluster".to_string(),
                    "local".to_string(),
                ])],
            },
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            labels: vec![("authz".to_string(), "tls-auth".to_string())]
                .into_iter()
                .collect(),
        }],
        labels: vec![("server".to_string(), "test".to_string())]
            .into_iter()
            .collect(),
    };

    let policies = Store::fixed(policy.clone(), None);
    let allowed = policies
        .check_policy(orig_dst_addr())
        .expect("port must be known");
    assert_eq!(*allowed.server, policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: Some(client_id()),
        negotiated_protocol: None,
    });
    assert_eq!(
        allowed
            .check_authorized(client_addr(), tls.clone())
            .expect("unauthenticated connection must be permitted"),
        Permitted {
            tls,
            protocol: policy.protocol,
            labels: vec![
                ("authz".to_string(), "tls-auth".to_string()),
                ("server".to_string(), "test".to_string())
            ]
            .into_iter()
            .collect()
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
        .check_authorized(client_addr(), tls)
        .expect_err("policy must require a client identity");
}

#[tokio::test(flavor = "current_thread")]
async fn tls_unauthenticated() {
    let policy = ServerPolicy {
        protocol: Protocol::Opaque,
        authorizations: vec![Authorization {
            authentication: Authentication::TlsUnauthenticated,
            networks: vec!["192.0.2.0/24".parse().unwrap()],
            labels: vec![("authz".to_string(), "tls-unauth".to_string())]
                .into_iter()
                .collect(),
        }],
        labels: vec![("server".to_string(), "test".to_string())]
            .into_iter()
            .collect(),
    };

    let policies = Store::fixed(policy.clone(), None);
    let allowed = policies
        .check_policy(orig_dst_addr())
        .expect("port must be known");
    assert_eq!(*allowed.server, policy);

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
        client_id: None,
        negotiated_protocol: None,
    });
    assert_eq!(
        allowed
            .check_authorized(client_addr(), tls.clone())
            .expect("unauthenticated connection must be permitted"),
        Permitted {
            tls,
            protocol: policy.protocol,
            labels: vec![
                ("authz".to_string(), "tls-unauth".to_string()),
                ("server".to_string(), "test".to_string())
            ]
            .into_iter()
            .collect()
        }
    );

    let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Passthru {
        sni: "othersa.testns.serviceaccount.identity.linkerd.cluster.example.com"
            .parse()
            .unwrap(),
    });
    allowed
        .check_authorized(client_addr(), tls)
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
