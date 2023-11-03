use crate::{direct, policy, Inbound};
use futures::Stream;
use linkerd_app_core::{
    exp_backoff::ExponentialBackoff,
    io, profiles,
    proxy::http,
    serve, svc,
    transport::{self, addrs::*},
    Error, Result,
};
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
    ) -> impl policy::GetPolicy + Clone + Send + Sync + 'static
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: http::HttpBody<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        self.config.policy.clone().build(workload, client, backoff)
    }

    pub async fn serve<A, I, G, GSvc, P>(
        self,
        addr: Local<ServerAddr>,
        listen: impl Stream<Item = Result<(A, I)>> + Send + Sync + 'static,
        policies: impl policy::GetPolicy + Clone + Send + Sync + 'static,
        profiles: P,
        gateway: G,
    ) where
        A: svc::Param<Remote<ClientAddr>>,
        A: svc::Param<OrigDstAddr>,
        A: svc::Param<AddrPair>,
        A: Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        G: svc::NewService<direct::GatewayTransportHeader, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<io::ScopedIo<I>>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<Error = Error>,
    {
        let shutdown = self.runtime.drain.clone().signaled();

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
        let server = http
            .push_http_tcp_server()
            .push_detect(forward)
            .push_accept(addr.port(), policies, direct)
            .into_inner();

        serve::serve(listen, server, shutdown).await;
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
