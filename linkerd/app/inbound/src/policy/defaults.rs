use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_server_policy::{Authentication, Authorization, Meta, Protocol, ServerPolicy, Suffix};
use std::{sync::Arc, time::Duration};

pub fn all_authenticated(timeout: Duration) -> ServerPolicy {
    mk("all-authenticated", all_nets(), authenticated(), timeout)
}

pub fn all_unauthenticated(timeout: Duration) -> ServerPolicy {
    mk(
        "all-unauthenticated",
        all_nets(),
        Authentication::Unauthenticated,
        timeout,
    )
}

pub fn cluster_authenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    mk("cluster-authenticated", nets, authenticated(), timeout)
}

pub fn cluster_unauthenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    mk(
        "cluster-unauthenticated",
        nets,
        Authentication::Unauthenticated,
        timeout,
    )
}

pub fn all_mtls_unauthenticated(timeout: Duration) -> ServerPolicy {
    mk(
        "all-tls-unauthenticated",
        all_nets(),
        Authentication::TlsUnauthenticated,
        timeout,
    )
}

fn all_nets() -> impl Iterator<Item = IpNet> {
    vec![Ipv4Net::default().into(), Ipv6Net::default().into()].into_iter()
}

fn authenticated() -> Authentication {
    Authentication::TlsAuthenticated {
        identities: Default::default(),
        suffixes: vec![Suffix::from(vec![])],
    }
}

fn mk(
    name: &'static str,
    nets: impl IntoIterator<Item = IpNet>,
    authentication: Authentication,
    timeout: Duration,
) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: nets.into_iter().map(Into::into).collect(),
            authentication,
            meta: Arc::new(Meta {
                group: "default".into(),
                kind: "default".into(),
                name: name.into(),
            }),
        }],
        meta: Arc::new(Meta {
            group: "default".into(),
            kind: "default".into(),
            name: name.into(),
        }),
    }
}
