use super::opaque_transport::OpaqueTransport;
use crate::{target::Endpoint, Outbound};
use linkerd_app_core::{
    io, svc, tls, transport::ConnectTcp, transport_header::SessionProtocol, Error, ProxyRuntime,
};
use tracing::debug_span;

impl Outbound<ConnectTcp> {
    pub fn new_tcp_connect(config: crate::Config, runtime: ProxyRuntime) -> Self {
        let stack = svc::stack(ConnectTcp::new(config.proxy.connect.keepalive));
        Self {
            config,
            runtime,
            stack,
        }
    }
}

impl<C> Outbound<C> {
    pub fn push_tcp_endpoint<P>(
        self,
        server_port: u16,
    ) -> Outbound<
        impl svc::Service<
                Endpoint<P>,
                Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        Endpoint<P>: svc::stack::Param<Option<SessionProtocol>>,
        C: svc::Service<Endpoint<P>, Error = io::Error> + Clone + Send + 'static,
        C::Response: tls::HasNegotiatedProtocol,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
        C::Future: Send + 'static,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
        let identity_disabled = rt.identity.is_none();

        let stack = connect
            // Initiates mTLS if the target is configured with identity. The
            // endpoint configures ALPN when there is an opaque transport hint OR
            // when an authority override is present (indicating the target is a
            // remote cluster gateway).
            .push(tls::Client::layer(rt.identity.clone()))
            // Encodes a transport header if the established connection is TLS'd and
            // ALPN negotiation indicates support.
            .push(OpaqueTransport::layer())
            // Limits the time we wait for a connection to be established.
            .push_timeout(config.proxy.connect.timeout)
            .push(svc::stack::BoxFuture::layer())
            .push(rt.metrics.transport.layer_connect())
            .push_map_target(move |e: Endpoint<P>| {
                if identity_disabled {
                    e.identity_disabled()
                } else {
                    e
                }
            })
            .push_request_filter(PreventLoop { port: server_port });

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }

    pub fn push_tcp_forward<I>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                super::Endpoint,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        C: svc::Service<super::Endpoint> + Clone + Send + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
        C::Error: Into<Error>,
        C::Future: Send,
    {
        let Self {
            config,
            runtime,
            stack: connect,
        } = self;

        let stack = connect
            .push_make_thunk()
            .push_on_response(super::Forward::layer())
            .instrument(|_: &_| debug_span!("tcp.forward"))
            .check_new_service::<super::Endpoint, I>();

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}

/// A connection policy that fails connections that target the outbound listener.
#[derive(Clone)]
struct PreventLoop {
    port: u16,
}

#[derive(Clone, Debug)]
struct LoopPrevented {
    port: u16,
}

// === impl PreventLoop ===

impl<P> svc::stack::Predicate<Endpoint<P>> for PreventLoop {
    type Request = Endpoint<P>;

    fn check(&mut self, ep: Endpoint<P>) -> Result<Endpoint<P>, Error> {
        let addr = ep.addr;

        tracing::trace!(%addr, self.port, "PreventLoop");
        if addr.ip().is_loopback() && addr.port() == self.port {
            return Err(LoopPrevented { port: self.port }.into());
        }

        Ok(ep)
    }
}

// === impl LoopPrevented ===

pub fn is_loop(err: &(dyn std::error::Error + 'static)) -> bool {
    err.is::<LoopPrevented>() || err.source().map(is_loop).unwrap_or(false)
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "outbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
