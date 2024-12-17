use crate::{direct, policy, Inbound};
use linkerd_app_core::{
    exp_backoff::ExponentialBackoff,
    io, profiles,
    proxy::http,
    svc,
    transport::{self, addrs::*},
    Error,
};
use linkerd_tonic_stream::ReceiveLimits;
use std::{fmt::Debug, sync::Arc};
use tracing::debug_span;

#[derive(Copy, Clone, Debug)]
struct TcpEndpoint {
    addr: Remote<ServerAddr>,
}

// === impl Inbound ===

impl Inbound<()> {
    pub fn build_policies<C>(
        &self,
        workload: Arc<str>,
        client: C,
        backoff: ExponentialBackoff,
        limits: ReceiveLimits,
    ) -> impl policy::GetPolicy + Clone + Send + Sync + 'static
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: http::Body<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        self.config
            .policy
            .clone()
            .build(workload, client, backoff, limits)
    }

    pub fn mk<A, I, P>(
        self,
        addr: Local<ServerAddr>,
        policies: impl policy::GetPolicy + Clone + Send + Sync + 'static,
        profiles: P,
        gateway: svc::ArcNewTcp<direct::GatewayTransportHeader, direct::GatewayIo<I>>,
    ) -> svc::ArcNewTcp<A, I>
    where
        A: svc::Param<Remote<ClientAddr>>,
        A: svc::Param<OrigDstAddr>,
        A: svc::Param<AddrPair>,
        A: Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        P: profiles::GetProfile<Error = Error>,
    {
        // Handles connections to ports that can't be determined to be HTTP.
        let forward = self
            .clone()
            .into_tcp_connect(addr.port())
            .push_tcp_forward()
            .into_stack()
            .push_map_target(TcpEndpoint::from_param)
            .instrument(|_: &_| debug_span!("tcp"))
            .into_inner();

        // Handles connections that target the inbound proxy port.
        let direct = {
            // Handles opaque connections that specify an HTTP session protocol.
            // This is identical to the primary HTTP stack (below), but it uses different
            // target & I/O types.
            let http = self
                .clone()
                .into_tcp_connect(addr.port())
                .push_http_router(profiles.clone())
                .push_http_server()
                .push_http_tcp_server()
                .into_inner();

            self.clone()
                .into_tcp_connect(addr.port())
                .push_tcp_forward()
                .map_stack(|_, _, s| s.push_map_target(TcpEndpoint::from_param))
                .push_direct(policies.clone(), gateway, http)
                .into_stack()
                .instrument(|_: &_| debug_span!("direct"))
                .into_inner()
        };

        // Handles HTTP connections.
        let http = self
            .into_tcp_connect(addr.port())
            .push_http_router(profiles)
            .push_http_server();

        // Determines how to handle an inbound connection, dispatching it to the appropriate
        // stack.
        http.push_http_tcp_server()
            .push_detect(forward)
            .push_accept(addr.port(), policies, direct)
            .into_inner()
    }
}

// === impl TcpEndpoint ===

impl TcpEndpoint {
    pub fn from_param<T: svc::Param<Remote<ServerAddr>>>(t: T) -> Self {
        Self { addr: t.param() }
    }
}

impl svc::Param<Remote<ServerAddr>> for TcpEndpoint {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl svc::Param<transport::labels::Key> for TcpEndpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundClient
    }
}
