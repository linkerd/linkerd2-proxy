use crate::{http, logical::Concrete, tcp, Outbound};
use linkerd_app_core::{
    io, metrics,
    profiles::LogicalAddr,
    proxy::{api_resolve::Metadata, resolve::map_endpoint::MapEndpoint},
    svc, tls,
    transport::{self, addrs::*},
    transport_header, Conditional, Error,
};
use std::{fmt, net::SocketAddr};

#[derive(Clone, Debug)]
pub struct Endpoint<P> {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub logical_addr: Option<LogicalAddr>,
    pub protocol: P,
    pub opaque_protocol: bool,
}

#[derive(Copy, Clone)]
pub struct FromMetadata {
    pub identity_disabled: bool,
}

// === impl Endpoint ===

impl Endpoint<()> {
    pub(crate) fn forward(addr: OrigDstAddr, reason: tls::NoClientTls) -> Self {
        Self {
            addr: Remote(ServerAddr(addr.into())),
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            opaque_protocol: false,
            protocol: (),
        }
    }

    pub(crate) fn from_metadata(
        addr: impl Into<SocketAddr>,
        metadata: Metadata,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
    ) -> Self {
        Self {
            addr: Remote(ServerAddr(addr.into())),
            tls: FromMetadata::client_tls(&metadata, reason),
            metadata,
            logical_addr: None,
            opaque_protocol,
            protocol: (),
        }
    }
}

impl<P> svc::Param<Remote<ServerAddr>> for Endpoint<P> {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl<P> svc::Param<tls::ConditionalClientTls> for Endpoint<P> {
    fn param(&self) -> tls::ConditionalClientTls {
        self.tls.clone()
    }
}

impl<P> svc::Param<Option<http::detect::Skip>> for Endpoint<P> {
    fn param(&self) -> Option<http::detect::Skip> {
        if self.opaque_protocol {
            Some(http::detect::Skip)
        } else {
            None
        }
    }
}

impl<P> svc::Param<Option<tcp::opaque_transport::PortOverride>> for Endpoint<P> {
    fn param(&self) -> Option<tcp::opaque_transport::PortOverride> {
        self.metadata
            .opaque_transport_port()
            .map(tcp::opaque_transport::PortOverride)
    }
}

impl<P> svc::Param<Option<http::AuthorityOverride>> for Endpoint<P> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl<P> svc::Param<transport::labels::Key> for Endpoint<P> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundConnect(self.param())
    }
}

impl<P> svc::Param<metrics::OutboundEndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = self
            .logical_addr
            .as_ref()
            .map(|LogicalAddr(a)| a.as_http_authority());
        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.tls.clone(),
            target_addr: self.addr.into(),
        }
    }
}

impl<P> svc::Param<metrics::EndpointLabels> for Endpoint<P> {
    fn param(&self) -> metrics::EndpointLabels {
        svc::Param::<metrics::OutboundEndpointLabels>::param(self).into()
    }
}

impl<P: std::hash::Hash> std::hash::Hash for Endpoint<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.tls.hash(state);
        self.logical_addr.hash(state);
        self.protocol.hash(state);
    }
}

// === EndpointFromMetadata ===

impl FromMetadata {
    fn client_tls(metadata: &Metadata, reason: tls::NoClientTls) -> tls::ConditionalClientTls {
        // If we're transporting an opaque protocol OR we're communicating with
        // a gateway, then set an ALPN value indicating support for a transport
        // header.
        let use_transport_header =
            metadata.opaque_transport_port().is_some() || metadata.authority_override().is_some();

        metadata
            .identity()
            .cloned()
            .map(move |server_id| {
                Conditional::Some(tls::ClientTls {
                    server_id,
                    alpn: if use_transport_header {
                        Some(tls::client::AlpnProtocols(vec![
                            transport_header::PROTOCOL.into()
                        ]))
                    } else {
                        None
                    },
                })
            })
            .unwrap_or(Conditional::None(reason))
    }
}

impl<P: Copy + std::fmt::Debug> MapEndpoint<Concrete<P>, Metadata> for FromMetadata {
    type Out = Endpoint<P>;

    fn map_endpoint(
        &self,
        concrete: &Concrete<P>,
        addr: SocketAddr,
        metadata: Metadata,
    ) -> Self::Out {
        tracing::trace!(%addr, ?metadata, ?concrete, "Resolved endpoint");
        let tls = if self.identity_disabled {
            tls::ConditionalClientTls::None(tls::NoClientTls::Disabled)
        } else {
            Self::client_tls(&metadata, tls::NoClientTls::NotProvidedByServiceDiscovery)
        };
        Endpoint {
            addr: Remote(ServerAddr(addr)),
            tls,
            metadata,
            logical_addr: Some(concrete.logical.logical_addr.clone()),
            protocol: concrete.logical.protocol,
            // XXX We never do protocol detection after resolving a concrete address to endpoints.
            // We should differentiate these target types statically.
            opaque_protocol: false,
        }
    }
}

// === Outbound ===

impl<S> Outbound<S> {
    pub fn push_endpoint<I>(
        self,
    ) -> Outbound<svc::BoxNewService<tcp::Endpoint, svc::BoxService<I, (), Error>>>
    where
        Self: Clone + 'static,
        S: svc::Service<tcp::Connect, Error = io::Error> + Clone + Send + Sync + Unpin + 'static,
        S::Response:
            tls::HasNegotiatedProtocol + io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        S::Future: Send + Unpin,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: fmt::Debug + Send + Sync + Unpin + 'static,
    {
        let http = self
            .clone()
            .push_tcp_endpoint::<http::Endpoint>()
            .push_http_endpoint()
            .push_http_server()
            .into_inner();

        self.push_tcp_endpoint()
            .push_tcp_forward()
            .push_detect_http(http)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::test_util::*;
    use hyper::{client::conn::Builder as ClientBuilder, Body, Request};
    use linkerd_app_core::svc::{NewService, Service, ServiceExt};
    use tokio::time;

    /// Tests that socket errors cause HTTP clients to be disconnected.
    #[tokio::test(flavor = "current_thread")]
    async fn propagates_http_errors() {
        let _trace = linkerd_tracing::test::trace_init();
        time::pause();

        let (rt, shutdown) = runtime();

        let (mut client, task) = {
            let addr = SocketAddr::new([10, 0, 0, 41].into(), 5550);
            let stack = Outbound::new(default_config(), rt)
                // Fails connection attempts
                .with_stack(support::connect().endpoint_fn(addr, |_| {
                    Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "i don't like you, go away",
                    ))
                }))
                .push_endpoint()
                .into_inner()
                .new_service(tcp::Endpoint::forward(
                    OrigDstAddr(addr),
                    tls::NoClientTls::Disabled,
                ));

            let (client_io, server_io) = support::io::duplex(4096);
            tokio::spawn(async move {
                let res = stack.oneshot(server_io).err_into::<Error>().await;
                tracing::info!(?res, "Server complete");
                res
            });

            let (client, conn) = ClientBuilder::new().handshake(client_io).await.unwrap();
            let client_task = tokio::spawn(async move {
                let res = conn.await;
                tracing::info!(?res, "Client complete");
                res
            });
            (client, client_task)
        };

        let status = {
            let req = Request::builder().body(Body::default()).unwrap();
            let rsp = client.ready().await.unwrap().call(req).await.unwrap();
            rsp.status()
        };
        assert_eq!(status, http::StatusCode::BAD_GATEWAY);

        // Ensure the client task completes, indicating that it has been disconnected.
        time::resume();
        time::timeout(time::Duration::from_secs(10), task)
            .await
            .expect("Timeout")
            .expect("Client task must not fail")
            .expect("Client must close gracefully");
        drop((client, shutdown));
    }
}
