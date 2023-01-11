use linkerd_app_core::{IpNet, Ipv4Net, Ipv6Net};
use linkerd_proxy_policy::{
    authz::Suffix, http, opaque, server, Authentication, Authorization, Meta,
};
use std::{sync::Arc, time::Duration};

pub fn all_authenticated(timeout: Duration) -> server::Policy {
    mk("all-authenticated", all_nets(), authenticated(), timeout)
}

pub fn all_unauthenticated(timeout: Duration) -> server::Policy {
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
) -> server::Policy {
    mk("cluster-authenticated", nets, authenticated(), timeout)
}

pub fn cluster_unauthenticated(
    nets: impl IntoIterator<Item = IpNet>,
    timeout: Duration,
) -> server::Policy {
    mk(
        "cluster-unauthenticated",
        nets,
        Authentication::Unauthenticated,
        timeout,
    )
}

pub fn all_mtls_unauthenticated(timeout: Duration) -> server::Policy {
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
) -> server::Policy {
    let authorizations = Arc::new([Authorization {
        meta: Meta::new_default(name),
        networks: nets.into_iter().map(Into::into).collect(),
        authentication,
    }]);

    // The default policy supports protocol detection and uses the default
    // authorization policy on a default route that matches all requests.
    let protocol = server::Protocol::Detect {
        timeout,
        http: Arc::new([http::default(authorizations.clone())]),
        opaque: Arc::new([opaque::default(authorizations)]),
    };

    server::Policy {
        meta: Meta::new_default(name),
        protocol,
    }
}
