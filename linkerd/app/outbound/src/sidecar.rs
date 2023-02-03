use crate::{http, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Error,
};
use std::fmt::Debug;
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sidecar<T>(T);

impl Outbound<()> {
    pub fn mk_sidecar<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: svc::Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R: Clone + Send + Sync + Unpin + 'static,
        P: profiles::GetProfile<Error = Error>,
    {
        let opaque = self.to_tcp_connect().push_opaque(resolve.clone());
        let http = self.to_tcp_connect().push_http(resolve);
        opaque
            .push_protocol(http.into_inner())
            .map_stack(|_, _, stk| stk.push_map_target(Sidecar))
            .push_discover(profiles)
            .push_discover_cache()
            .push_tcp_instrument(|t: &T| info_span!("proxy", addr = %t.param()))
            .into_inner()
    }
}

impl<T> svc::Param<OrigDstAddr> for Sidecar<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.0.param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Sidecar<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.0.param();
        Remote(ServerAddr(addr))
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Sidecar<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.0.param()
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Sidecar<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
{
    fn param(&self) -> Option<profiles::Receiver> {
        self.0.param()
    }
}

impl<T> svc::Param<Option<http::detect::Skip>> for Sidecar<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
{
    fn param(&self) -> Option<http::detect::Skip> {
        if let Some(profile) = self.0.param() {
            if profile.is_opaque_protocol() {
                return Some(http::detect::Skip);
            }
        }

        None
    }
}

impl<T> svc::Param<http::logical::Target> for Sidecar<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
    Self: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> http::logical::Target {
        if let Some(profile) = self.target.param() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return http::logical::Target::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return http::logical::Target::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        http::logical::Target::Forward(self.param(), Default::default())
    }
}
