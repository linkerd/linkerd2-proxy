use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::time::Duration;

pub fn all_authenticated(timeout: Duration) -> ServerPolicy {
    authenticated("default:all-authenticated", all_nets(), timeout)
}

pub fn all_unauthenticated(timeout: Duration) -> ServerPolicy {
    unauthenticated("default:all-unauthenticated", all_nets(), timeout)
}

pub fn cluster_authenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    authenticated("default:cluster-authenticated", nets, timeout)
}

pub fn cluster_unauthenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    unauthenticated("default:cluster-unauthenticated", nets, timeout)
}

pub fn all_mtls_unauthenticated(timeout: Duration) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: all_nets().map(Into::into).collect(),
            authentication: Authentication::TlsUnauthenticated,
            labels: Some((
                "name".to_string(),
                "default:all-tls-unauthenticated".to_string(),
            ))
            .into_iter()
            .collect(),
        }],
        labels: Some(("name".to_string(), "default:all-tls-unauthenticated".into()))
            .into_iter()
            .collect(),
    }
}

fn all_nets() -> impl Iterator<Item = IpNet> {
    vec![Ipv4Net::default().into(), Ipv6Net::default().into()].into_iter()
}

fn authenticated(
    name: impl Into<String>,
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    let name = name.into();
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: nets.into_iter().map(Into::into).collect(),
            authentication: Authentication::TlsAuthenticated {
                identities: Default::default(),
                suffixes: vec![Suffix::from(vec![])],
            },
            labels: Some(("name".to_string(), name.clone()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("name".to_string(), name)).into_iter().collect(),
    }
}

fn unauthenticated(
    name: impl Into<String>,
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    let name = name.into();
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: nets.into_iter().map(Into::into).collect(),
            authentication: Authentication::Unauthenticated,
            labels: Some(("name".to_string(), name.clone()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("name".to_string(), name)).into_iter().collect(),
    }
}
