use super::opaque_transport::{self, OpaqueTransport};
use crate::{endpoint::EndpointError, ConnectMeta, Outbound};
use futures::future;
use linkerd_app_core::{
    io,
    proxy::http,
    svc, tls,
    transport::{self, ClientAddr, ConnectTcp, Local, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error,
};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct Connect {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
}

/// Prevents outbound connections on the loopback interface, unless the
/// `allow-loopback` feature is enabled.
#[derive(Clone, Debug)]
pub struct PreventLoopback<S>(S);

// === impl Outbound ===

impl Outbound<()> {
    pub fn to_tcp_connect(&self) -> Outbound<PreventLoopback<ConnectTcp>> {
        let connect = PreventLoopback(ConnectTcp::new(self.config.proxy.connect.keepalive));
        self.clone().with_stack(connect)
    }
}

impl<C> Outbound<C> {
    pub fn push_tcp_endpoint<T>(
        self,
    ) -> Outbound<
        impl svc::MakeConnection<
                T,
                Connection = impl Send + Unpin,
                Metadata = ConnectMeta,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >
    where
        T: svc::Param<Remote<ServerAddr>>
            + svc::Param<tls::ConditionalClientTls>
            + svc::Param<Option<opaque_transport::PortOverride>>
            + svc::Param<Option<http::AuthorityOverride>>
            + svc::Param<Option<SessionProtocol>>
            + svc::Param<transport::labels::Key>,
        C: svc::MakeConnection<Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send + 'static,
    {
        self.map_stack(|config, rt, connect| {
            connect
                // Initiates mTLS if the target is configured with identity. The
                // endpoint configures ALPN when there is an opaque transport hint OR
                // when an authority override is present (indicating the target is a
                // remote cluster gateway).
                .push(tls::Client::layer(rt.identity.clone()))
                // Encodes a transport header if the established connection is TLS'd and
                // ALPN negotiation indicates support.
                .push(OpaqueTransport::layer())
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(config.proxy.connect.timeout)
                .push(svc::stack::BoxFuture::layer())
                .push(transport::metrics::Client::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
        })
    }

    pub fn push_tcp_forward<T, I>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = EndpointError, Future = impl Send> + Clone,
        >,
    >
    where
        T: svc::Param<Remote<ServerAddr>>
            + svc::Param<Option<http::Version>>
            + Clone
            + Send
            + Sync
            + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        C: svc::MakeConnection<T> + Clone + Send + Sync + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
    {
        self.map_stack(|_, _, conn| {
            conn.push(svc::stack::WithoutConnectionMetadata::layer())
                .push_make_thunk()
                .push_on_service(super::Forward::layer())
                .push(svc::NewAnnotateError::layer_from_target())
                .push(svc::ArcNewService::layer())
                .check_new_service::<T, I>()
        })
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

// === impl Connect ===

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
mod tests {
    use super::*;
    use crate::{
        svc::{self, NewService, ServiceExt},
        test_util::*,
        transport::{ClientAddr, Local},
    };
    use std::net::SocketAddr;

    #[derive(Clone)]
    struct Target(SocketAddr);

    impl svc::Param<Option<crate::http::Version>> for Target {
        fn param(&self) -> Option<crate::http::Version> {
            None
        }
    }

    impl svc::Param<Remote<ServerAddr>> for Target {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(self.0))
        }
    }

    #[tokio::test]
    async fn forward() {
        let _trace = linkerd_tracing::test::trace_init();

        let addr = SocketAddr::new([192, 0, 2, 2].into(), 2222);
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(svc::mk(move |Target(a): Target| {
                assert_eq!(a, addr);
                let mut io = support::io();
                io.write(b"hello").read(b"world");
                future::ok::<_, support::io::Error>((
                    io.build(),
                    Local(ClientAddr(([0, 0, 0, 0], 0).into())),
                ))
            }))
            .push_tcp_forward()
            .into_inner();

        let mut io = support::io();
        io.read(b"hello").write(b"world");
        stack
            .new_service(Target(addr))
            .oneshot(io.build())
            .await
            .expect("forward must complete successfully");
    }
}
