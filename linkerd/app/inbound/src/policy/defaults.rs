use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::time::Duration;

pub fn all_authenticated(timeout: Duration) -> ServerPolicy {
    mk(
        "default:all-authenticated",
        all_nets(),
        authenticated(),
        timeout,
    )
}

pub fn all_unauthenticated(timeout: Duration) -> ServerPolicy {
    mk(
        "default:all-unauthenticated",
        all_nets(),
        Authentication::Unauthenticated,
        timeout,
    )
}

pub fn cluster_authenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    mk(
        "default:cluster-authenticated",
        nets,
        authenticated(),
        timeout,
    )
}

pub fn cluster_unauthenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> ServerPolicy {
    mk(
        "default:cluster-unauthenticated",
        nets,
        Authentication::Unauthenticated,
        timeout,
    )
}

pub fn all_mtls_unauthenticated(timeout: Duration) -> ServerPolicy {
    mk(
        "default:all-tls-unauthenticated",
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
    name: &str,
    nets: impl IntoIterator<Item = IpNet>,
    authentication: Authentication,
    timeout: Duration,
) -> ServerPolicy {
    ServerPolicy {
        protocol: Protocol::Detect { timeout },
        authorizations: vec![
            Authorization {
                networks: nets.into_iter().map(Into::into).collect(),
                authentication,
                name: name.to_string(),
            },
            loopback_unauthenticated(),
        ],
        name: name.to_string(),
    }
}

pub(super) fn loopback_unauthenticated() -> Authorization {
    let v4 = "127.0.0.0/8"
        .parse::<IpNet>()
        .expect("loopback addresses must parse");
    let v6 = "::1/128"
        .parse::<IpNet>()
        .expect("loopback addresses must parse");
    Authorization {
        networks: vec![v4, v6].into_iter().map(Into::into).collect(),
        authentication: Authentication::Unauthenticated,
        name: "default:loopback".to_string(),
    }
}
