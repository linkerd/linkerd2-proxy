use crate::{direct, policy, ForwardError, Inbound};
use linkerd_app_core::{
    config::ConnectConfig,
    drain,
    exp_backoff::ExponentialBackoff,
    io, profiles,
    proxy::{http, tcp},
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
        C: tonic::client::GrpcService<tonic::body::Body, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: http::Body<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Send + 'static,
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
        let detect_metrics = self.runtime.metrics.detect.clone();

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
            .push_detect(detect_metrics, forward)
            .push_accept(addr.port(), policies, direct)
            .into_inner()
    }

    /// Readies the inbound stack to make TCP connections (for both TCP
    /// forwarding and HTTP proxying).
    fn into_tcp_connect<T>(
        self,
        proxy_port: u16,
    ) -> Inbound<
        impl svc::MakeConnection<
                T,
                Connection = impl Send + Unpin,
                Metadata = impl Send + Unpin,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        T: svc::Param<Remote<ServerAddr>> + 'static,
    {
        self.map_stack(|config, _, _| {
            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let ConnectConfig {
                ref keepalive,
                ref user_timeout,
                ref timeout,
                ..
            } = config.proxy.connect;

            #[derive(Debug, thiserror::Error)]
            #[error("inbound connection must not target port {0}")]
            struct Loop(u16);

            svc::stack(transport::ConnectTcp::new(*keepalive, *user_timeout))
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(*timeout)
                // Prevent connections that would target the inbound proxy port from looping.
                .push_filter(move |t: T| {
                    let addr = t.param();
                    let port = addr.port();
                    if port == proxy_port {
                        return Err(Loop(port));
                    }
                    Ok(addr)
                })
        })
    }
}

impl<S> Inbound<S> {
    // Forwards TCP streams that cannot be decoded as HTTP.
    //
    // Looping is always prevented.
    fn push_tcp_forward<T, I>(
        self,
    ) -> Inbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = ForwardError, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<transport::labels::Key>
            + svc::Param<Remote<ServerAddr>>
            + Clone
            + Send
            + Sync
            + 'static,
        I: io::AsyncRead + io::AsyncWrite,
        I: Debug + Send + Unpin + 'static,
        S: svc::MakeConnection<T> + Clone + Send + Sync + Unpin + 'static,
        S::Connection: Send + Unpin,
        S::Metadata: Send + Unpin,
        S::Future: Send,
    {
        self.map_stack(|_, rt, connect| {
            connect
                .push(transport::metrics::Client::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_new_thunk()
                .push_on_service(tcp::Forward::layer())
                .push_on_service(drain::Retain::layer(rt.drain.clone()))
                .instrument(|_: &_| debug_span!("tcp"))
                .push(svc::NewMapErr::layer_from_target::<ForwardError, _>())
                .push(svc::ArcNewService::layer())
                .check_new::<T>()
        })
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
