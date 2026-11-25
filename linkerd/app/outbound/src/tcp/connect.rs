use crate::{policy, Outbound};
use futures::future;
use linkerd_app_core::{
    io, svc, tls,
    transport::{addrs::*, ConnectTcp},
    IpMatch,
};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct Connect {
    addr: Remote<ServerAddr>,
    tls: tls::ConditionalClientTls,
}

/// Prevents outbound connections on the loopback interface, unless the
/// `allow-loopback` feature is enabled.
#[derive(Clone, Debug)]
pub struct PreventLoopback<S>(S);

/// Enforces the outbound default policy's mTLS requirement: refuses a cleartext
/// connection to a target that discovery did not associate with a mesh
/// identity.
///
/// Whether a target is meshed is expressed by its
/// [`tls::ConditionalClientTls`]: a meshed endpoint resolves to
/// `Some(..)` (mTLS), while an unmeshed one resolves to
/// `None(NotProvidedByServiceDiscovery)`. Only the latter is subject to
/// enforcement — other cleartext reasons (loopback, identity disabled, ingress
/// without discovery) are always permitted.
#[derive(Clone, Debug)]
pub struct RequireMeshIdentity<S> {
    inner: S,
    requirement: Requirement,
}

#[derive(Clone, Debug)]
enum Requirement {
    /// No enforcement; cleartext to unmeshed targets is permitted
    /// (`all-unauthenticated`).
    Disabled,
    /// Every unmeshed target is refused (`all-authenticated`).
    All,
    /// Only unmeshed targets within these networks are refused; targets outside
    /// the cluster are permitted (`cluster-authenticated`).
    WithinCluster(IpMatch),
}

// === impl Outbound ===

impl Outbound<()> {
    pub fn to_tcp_connect(&self) -> Outbound<PreventLoopback<RequireMeshIdentity<ConnectTcp>>> {
        let connect = ConnectTcp::new(
            self.config.proxy.connect.keepalive,
            self.config.proxy.connect.user_timeout,
        );
        let requirement = Requirement::new(
            self.config.default_policy,
            self.config.cluster_networks.clone(),
        );
        let connect = PreventLoopback(RequireMeshIdentity::new(connect, requirement));
        self.clone().with_stack(connect)
    }
}

// === impl PreventLoopback ===

impl<S> PreventLoopback<S> {
    #[cfg(not(feature = "allow-loopback"))]
    fn check_loopback(Remote(ServerAddr(addr)): Remote<ServerAddr>) -> io::Result<()> {
        if addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "Outbound proxy cannot initiate connections on the loopback interface",
            ));
        }

        Ok(())
    }

    #[cfg(feature = "allow-loopback")]
    // the Result is necessary to have the same type signature regardless of
    // whether or not the `allow-loopback` feature is enabled...
    fn check_loopback(_: Remote<ServerAddr>) -> io::Result<()> {
        Ok(())
    }
}

impl<T, S> svc::Service<T> for PreventLoopback<S>
where
    T: svc::Param<Remote<ServerAddr>>,
    S: svc::Service<T, Error = io::Error>,
{
    type Response = S::Response;
    type Error = io::Error;
    type Future = future::Either<S::Future, future::Ready<io::Result<S::Response>>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, ep: T) -> Self::Future {
        if let Err(e) = Self::check_loopback(ep.param()) {
            return future::Either::Right(future::err(e));
        }

        future::Either::Left(self.0.call(ep))
    }
}

// === impl RequireMeshIdentity ===

impl Requirement {
    fn new(policy: policy::DefaultPolicy, cluster_networks: IpMatch) -> Self {
        match policy {
            policy::DefaultPolicy::Allow => Self::Disabled,
            policy::DefaultPolicy::RequireIdentity => Self::All,
            policy::DefaultPolicy::RequireIdentityWithinCluster => {
                Self::WithinCluster(cluster_networks)
            }
        }
    }
}

impl<S> RequireMeshIdentity<S> {
    fn new(inner: S, requirement: Requirement) -> Self {
        Self { inner, requirement }
    }

    /// Returns `true` if a connection to `addr` with the given TLS status must
    /// be refused because the target has no mesh identity.
    fn refuses(
        &self,
        Remote(ServerAddr(addr)): Remote<ServerAddr>,
        tls: tls::ConditionalClientTls,
    ) -> bool {
        // Only refuse when discovery declined to provide a mesh identity for
        // the endpoint. Every other cleartext reason (loopback, identity
        // administratively disabled, ingress without discovery) is left alone.
        if !matches!(
            tls,
            tls::ConditionalClientTls::None(tls::NoClientTls::NotProvidedByServiceDiscovery)
        ) {
            return false;
        }

        match self.requirement {
            Requirement::Disabled => false,
            Requirement::All => true,
            Requirement::WithinCluster(ref nets) => nets.matches(addr.ip()),
        }
    }
}

impl<T, S> svc::Service<T> for RequireMeshIdentity<S>
where
    T: svc::Param<Remote<ServerAddr>>,
    T: svc::Param<tls::ConditionalClientTls>,
    S: svc::Service<T, Error = io::Error>,
{
    type Response = S::Response;
    type Error = io::Error;
    type Future = future::Either<S::Future, future::Ready<io::Result<S::Response>>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, ep: T) -> Self::Future {
        if self.refuses(ep.param(), ep.param()) {
            return future::Either::Right(future::err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "Refusing cleartext connection to a target without a mesh identity \
                 (the outbound default policy requires mTLS)",
            )));
        }

        future::Either::Left(self.inner.call(ep))
    }
}

// === impl Connect ===

impl Connect {
    pub fn new(addr: Remote<ServerAddr>, tls: tls::ConditionalClientTls) -> Self {
        Self { addr, tls }
    }
}

impl svc::Param<Remote<ServerAddr>> for Connect {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl svc::Param<tls::ConditionalClientTls> for Connect {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

#[cfg(test)]
impl Connect {
    pub fn addr(&self) -> &Remote<ServerAddr> {
        &self.addr
    }

    pub fn tls(&self) -> &tls::ConditionalClientTls {
        &self.tls
    }
}

#[cfg(test)]
mod require_mesh_identity_tests {
    use super::*;
    use linkerd_app_core::{svc::ServiceExt, IpNet};
    use std::net::SocketAddr;

    /// The discovery signal for an unmeshed endpoint.
    fn unmeshed() -> tls::ConditionalClientTls {
        tls::ConditionalClientTls::None(tls::NoClientTls::NotProvidedByServiceDiscovery)
    }

    /// The discovery signal for a meshed endpoint: discovery provided a mesh
    /// identity.
    fn meshed() -> tls::ConditionalClientTls {
        let server_id = tls::ServerId("server.id".parse().unwrap());
        let server_name = tls::ServerName("server.name".parse().unwrap());
        tls::ConditionalClientTls::Some(tls::ClientTls::new(server_id, server_name))
    }

    fn target(ip: [u8; 4], tls: tls::ConditionalClientTls) -> Connect {
        Connect::new(Remote(ServerAddr(SocketAddr::new(ip.into(), 8080))), tls)
    }

    async fn connect(requirement: Requirement, target: Connect) -> io::Result<()> {
        // A trivial inner connector that always succeeds, so the only outcome
        // that matters is whether the guard short-circuits.
        let inner = svc::mk(|_: Connect| future::ok::<(), io::Error>(()));
        RequireMeshIdentity::new(inner, requirement)
            .oneshot(target)
            .await
    }

    fn cluster() -> IpMatch {
        IpMatch::new(Some("10.0.0.0/8".parse::<IpNet>().unwrap()))
    }

    #[tokio::test]
    async fn all_refuses_unmeshed() {
        let err = connect(Requirement::All, target([192, 0, 2, 1], unmeshed()))
            .await
            .expect_err("unmeshed target must be refused");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn all_permits_meshed() {
        // The primary case the feature exists for: under `all-authenticated`,
        // a meshed target (discovery provided a mesh identity) still connects.
        connect(Requirement::All, target([10, 1, 2, 3], meshed()))
            .await
            .expect("meshed target must be permitted");
    }

    #[tokio::test]
    async fn all_permits_non_discovery_cleartext() {
        // Cleartext reasons other than `NotProvidedByServiceDiscovery` (here,
        // loopback) are never refused — the guard is specific to the
        // "discovery gave us no identity" case.
        connect(
            Requirement::All,
            target(
                [127, 0, 0, 1],
                tls::ConditionalClientTls::None(tls::NoClientTls::Loopback),
            ),
        )
        .await
        .expect("loopback (non-discovery) cleartext must be permitted");
    }

    #[tokio::test]
    async fn disabled_permits_unmeshed() {
        connect(Requirement::Disabled, target([192, 0, 2, 1], unmeshed()))
            .await
            .expect("with no enforcement, unmeshed targets connect");
    }

    #[tokio::test]
    async fn within_cluster_refuses_in_cluster_unmeshed() {
        let err = connect(
            Requirement::WithinCluster(cluster()),
            target([10, 1, 2, 3], unmeshed()),
        )
        .await
        .expect_err("in-cluster unmeshed target must be refused");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn within_cluster_permits_out_of_cluster_unmeshed() {
        // Egress outside the cluster networks is permitted even without a mesh
        // identity ("meshed OR outside the cluster").
        connect(
            Requirement::WithinCluster(cluster()),
            target([192, 0, 2, 1], unmeshed()),
        )
        .await
        .expect("out-of-cluster unmeshed egress must be permitted");
    }
}
