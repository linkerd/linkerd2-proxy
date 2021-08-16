use linkerd_app_core::{Ipv4Net, Ipv6Net};
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::time::Duration;

pub fn all_authenticated(timeout: Duration) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: vec![Ipv4Net::default().into(), Ipv6Net::default().into()],
            authentication: Authentication::TlsAuthenticated {
                identities: Default::default(),
                suffixes: vec![Suffix::from(vec![])],
            },
            labels: Some(("authz".to_string(), "_all-authenticated".to_string()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("server".to_string(), "_default".to_string()))
            .into_iter()
            .collect(),
    }
}

pub fn all_unauthenticated(timeout: Duration) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: vec![Ipv4Net::default().into(), Ipv6Net::default().into()],
            authentication: Authentication::Unauthenticated,
            labels: Some(("authz".to_string(), "_all-unauthenticated".to_string()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("server".to_string(), "_default".to_string()))
            .into_iter()
            .collect(),
    }
}

pub fn all_mtls_unauthenticated(timeout: Duration) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![Authorization {
            networks: vec![Ipv4Net::default().into(), Ipv6Net::default().into()],
            authentication: Authentication::TlsUnauthenticated,
            labels: Some(("authz".to_string(), "_all-unauthenticated-tls".to_string()))
                .into_iter()
                .collect(),
        }],
        labels: Some(("server".to_string(), "_default".to_string()))
            .into_iter()
            .collect(),
    }
}
