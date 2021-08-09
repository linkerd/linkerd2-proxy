use crate::{direct, Inbound};
use linkerd_app_core::{
    config::ServerConfig,
    io, profiles, serve, svc,
    transport::{self, listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr},
    Error,
};
use std::{fmt::Debug, future::Future};
use tracing::debug_span;

#[derive(Copy, Clone, Debug)]
struct TcpEndpoint {
    port: u16,
}

// === impl Inbound ===

impl Inbound<()> {
    pub fn serve<B, G, GSvc, P>(
        self,
        bind: B,
        profiles: P,
        gateway: G,
    ) -> (Local<ServerAddr>, impl Future<Output = ()> + Send)
    where
        B: Bind<ServerConfig>,
        B::Addrs: svc::Param<Remote<ClientAddr>>
            + svc::Param<Local<ServerAddr>>
            + svc::Param<OrigDstAddr>,
        G: svc::NewService<direct::GatewayConnection, Service = GSvc>,
        G: Clone + Send + Sync + Unpin + 'static,
        GSvc: svc::Service<direct::GatewayIo<io::ScopedIo<B::Io>>, Response = ()> + Send + 'static,
        GSvc::Error: Into<Error>,
        GSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let (Local(ServerAddr(la)), listen) = bind
            .bind(&self.config.proxy.server)
            .expect("Failed to bind inbound listener");

        let serve = async move {
            let shutdown = self.runtime.drain.clone().signaled();

            // Handles connections to ports that can't be determined to be HTTP.
            let forward = self
                .clone()
                .into_tcp_connect(la.port())
                .push_tcp_forward()
                .into_stack()
                .push_map_target(TcpEndpoint::from_param)
                .instrument(|_: &_| debug_span!("tcp"))
                .into_inner();

            // Handles connections that target the inbound proxy port.
            let direct = self
                .clone()
                .into_tcp_connect(la.port())
                .push_tcp_forward()
                .map_stack(|_, _, s| s.push_map_target(TcpEndpoint::from_param))
                .push_direct(gateway)
                .into_stack()
                .instrument(|_: &_| debug_span!("direct"))
                .into_inner();

            // Handles HTTP connections.
            let http = self
                .into_tcp_connect(la.port())
                .push_http_router(profiles)
                .push_http_server();

            // Determines how to handle an inbound connection, dispatching it to the appropriate
            // stack.
            let server = http
                .push_detect_http(forward.clone())
                .push_detect_tls(forward)
                .push_accept(la.port(), direct)
                .into_inner();

            serve::serve(listen, server, shutdown).await
        };

        (Local(ServerAddr(la)), serve)
    }
}

// === impl TcpEndpoint ===

impl TcpEndpoint {
    pub fn from_param<T: svc::Param<u16>>(t: T) -> Self {
        Self { port: t.param() }
    }
}

impl svc::Param<u16> for TcpEndpoint {
    fn param(&self) -> u16 {
        self.port
    }
}

impl svc::Param<transport::labels::Key> for TcpEndpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}
