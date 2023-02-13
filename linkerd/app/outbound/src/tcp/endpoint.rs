use super::{tagged_transport::TaggedTransport, *};
use crate::{ConnectMeta, Outbound};
use linkerd_app_core::{
    io,
    proxy::http,
    svc, tls,
    transport::{self, ClientAddr, Local, Remote, ServerAddr},
    transport_header::SessionProtocol,
    Error,
};

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
        // TCP endpoint target.
        T: svc::Param<Remote<ServerAddr>>,
        T: svc::Param<tls::ConditionalClientTls>,
        T: svc::Param<Option<tagged_transport::PortOverride>>,
        T: svc::Param<Option<http::AuthorityOverride>>,
        T: svc::Param<Option<SessionProtocol>>,
        T: svc::Param<transport::labels::Key>,
        // Connector stack.
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
                .push(TaggedTransport::layer())
                // Limits the time we wait for a connection to be established.
                .push_connect_timeout(config.proxy.connect.timeout)
                .push(svc::stack::BoxFuture::layer())
                .push(transport::metrics::Client::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
        })
    }
}
