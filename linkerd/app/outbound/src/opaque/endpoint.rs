use crate::{
    tcp::{
        self,
        opaque_transport::{self, OpaqueTransport},
    },
    ConnectMeta, Outbound,
};
use linkerd_app_core::{
    io,
    proxy::http,
    svc, tls,
    transport::{self, ClientAddr, Local, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error,
};

impl<C> Outbound<C> {
    pub fn push_opaque_endpoint<T>(
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
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
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
}
