use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_proxy_server_policy::{
    authz::Suffix, http, Authentication, Authorization, Meta, Protocol, ServerPolicy,
};
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
    let authorizations = Arc::new([Authorization {
        meta: Meta::new_default(name),
        networks: nets.into_iter().map(Into::into).collect(),
        authentication,
    }]);

    // The default policy supports protocol detection and uses the default
    // authorization policy on a default route that matches all requests.
    let protocol = Protocol::Detect {
        timeout,
        http: Arc::new([http::default(authorizations.clone())]),
        tcp_authorizations: authorizations,
    };

    ServerPolicy {
        meta: Meta::new_default(name),
        protocol,
    }
}
