use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::time::Duration;

fn all_nets() -> impl Iterator<Item = IpNet> {
    vec![Ipv4Net::default().into(), Ipv6Net::default().into()].into_iter()
}

pub fn all_authenticated(name: impl Into<String>, timeout: Duration) -> ServerPolicy {
    authenticated(name, all_nets(), timeout)
}

pub fn all_unauthenticated(name: impl Into<String>, timeout: Duration) -> ServerPolicy {
    unauthenticated(name, all_nets(), timeout)
}

pub fn authenticated(
    name: impl Into<String>,
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: nets.into_iter().map(Into::into).collect(),
            authentication: Authentication::TlsAuthenticated {
                identities: Default::default(),
                suffixes: vec![Suffix::from(vec![])],
            },
            labels: Some(("name".to_string(), "default:all-authenticated".to_string()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("name".to_string(), name.into()))
            .into_iter()
            .collect(),
    }
}

pub fn unauthenticated(
    name: impl Into<String>,
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: nets.into_iter().map(Into::into).collect(),
            authentication: Authentication::Unauthenticated,
            labels: Some((
                "name".to_string(),
                "default:all-unauthenticated".to_string(),
            ))
            .into_iter()
            .collect(),
        }],
        labels: Some(("name".to_string(), name.into()))
            .into_iter()
            .collect(),
    }
}

pub fn all_mtls_unauthenticated(name: impl Into<String>, timeout: Duration) -> ServerPolicy {
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
        labels: Some(("name".to_string(), name.into()))
            .into_iter()
            .collect(),
    }
}
