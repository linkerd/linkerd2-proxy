use crate::{http, tcp, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc::{self, stack::Param},
    transport::addrs::*,
    Error,
};
use std::fmt::Debug;
use tracing::info_span;

impl Outbound<()> {
    pub(crate) fn mk_sidecar<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Resolve<tcp::Concrete, Endpoint = Metadata, Error = Error>,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        P: profiles::GetProfile<Error = Error>,
    {
        let logical = self.to_tcp_connect().push_logical(resolve);
        let forward = self.to_tcp_connect().push_forward();
        forward
            .push_switch_logical(logical.into_inner())
            .push_discover(profiles)
            .push_discover_cache()
            .push_tcp_instrument(|t: &T| info_span!("proxy", addr = %t.param()))
            .into_inner()
    }
}
