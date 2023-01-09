use crate::{http, logical::Concrete, stack_labels, tcp, Outbound};
use linkerd_app_core::{
    io, metrics,
    profiles::LogicalAddr,
    proxy::api_resolve::Metadata,
    svc, tls,
    transport::{self, addrs::*},
    transport_header, Conditional,
};
use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint<P> {
    pub addr: Remote<ServerAddr>,
    pub tls: tls::ConditionalClientTls,
    pub metadata: Metadata,
    pub logical_addr: Option<LogicalAddr>,
    pub protocol: P,
    pub opaque_protocol: bool,
}

#[derive(Clone, Debug)]
pub struct NewFromMetadata<N> {
    inbound_ips: Arc<HashSet<IpAddr>>,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct FromMetadata<P, N> {
    inbound_ips: Arc<HashSet<IpAddr>>,
    concrete: Concrete<P>,
    inner: N,
}

// === impl Endpoint ===

impl Endpoint<()> {
    pub(crate) fn forward(
        addr: OrigDstAddr,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
    ) -> Self {
        Self {
            addr: Remote(ServerAddr(addr.into())),
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            opaque_protocol,
            protocol: (),
        }
    }

    pub fn from_metadata(
        addr: impl Into<SocketAddr>,
        mut metadata: Metadata,
        reason: tls::NoClientTls,
        opaque_protocol: bool,
        inbound_ips: &HashSet<IpAddr>,
    ) -> Self {
        let addr: SocketAddr = addr.into();
        let tls = if inbound_ips.contains(&addr.ip()) {
            metadata.clear_upgrade();
            tracing::debug!(%addr, ?metadata, ?addr, ?inbound_ips, "Target is local");
            tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
        } else {
            client_tls(&metadata, reason)
        };

        Self {
            addr: Remote(ServerAddr(addr)),
            tls,
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
        transport::labels::Key::OutboundClient(self.param())
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

// === NewFromMetadata ===

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

impl<N> NewFromMetadata<N> {
    pub fn new(inbound_ips: Arc<HashSet<IpAddr>>, inner: N) -> Self {
        Self { inbound_ips, inner }
    }

    pub fn layer(inbound_ips: Arc<HashSet<IpAddr>>) -> impl svc::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self::new(inbound_ips.clone(), inner))
    }
}

impl<P, N> svc::NewService<Concrete<P>> for NewFromMetadata<N>
where
    P: Copy + std::fmt::Debug,
    N: svc::NewService<Concrete<P>>,
{
    type Service = FromMetadata<P, N::Service>;

    fn new_service(&self, concrete: Concrete<P>) -> Self::Service {
        FromMetadata {
            inner: self.inner.new_service(concrete.clone()),
            concrete,
            inbound_ips: self.inbound_ips.clone(),
        }
    }
}

impl<P, N> svc::NewService<(SocketAddr, Metadata)> for FromMetadata<P, N>
where
    P: Copy + std::fmt::Debug,
    N: svc::NewService<Endpoint<P>>,
{
    type Service = N::Service;

    fn new_service(&self, (addr, mut metadata): (SocketAddr, Metadata)) -> Self::Service {
        tracing::trace!(%addr, ?metadata, concrete = ?self.concrete, "Resolved endpoint");
        let tls = if self.inbound_ips.contains(&addr.ip()) {
            metadata.clear_upgrade();
            tracing::debug!(%addr, ?metadata, ?addr, ?self.inbound_ips, "Target is local");
            tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
        } else {
            client_tls(&metadata, tls::NoClientTls::NotProvidedByServiceDiscovery)
        };
        self.inner.new_service(Endpoint {
            addr: Remote(ServerAddr(addr)),
            tls,
            metadata,
            logical_addr: Some(self.concrete.logical.logical_addr.clone()),
            protocol: self.concrete.logical.protocol,
            // XXX We never do protocol detection after resolving a concrete address to endpoints.
            // We should differentiate these target types statically.
            opaque_protocol: false,
        })
    }
}

// === Outbound ===

impl<S> Outbound<S> {
    /// Builds a stack that handles forwarding a connection to a single endpoint
    /// (i.e. without routing and load balancing).
    ///
    /// HTTP protocol detection may still be performed on the connection.
    ///
    /// A service produced by this stack is used for a single connection (i.e.
    /// without any form of caching for reuse across connections).
    pub fn push_forward<I>(self) -> Outbound<svc::ArcNewTcp<tcp::Endpoint, I>>
    where
        Self: Clone + 'static,
        S: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        S: Clone + Send + Sync + Unpin + 'static,
        S::Connection: Send + Unpin + 'static,
        S::Future: Send,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: fmt::Debug + Send + Sync + Unpin + 'static,
    {
        // The forwarding stacks are **NOT** cached, since they don't do any
        // external discovery.
        let http = self
            .clone()
            .push_tcp_endpoint::<http::Connect>()
            .push_http_endpoint()
            .map_stack(|config, rt, stk| {
                stk.push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "forward")),
                        )
                        // TODO(ver): This buffer config should be distinct from
                        // that in the concrete stack. It should probably be
                        // derived from the target so that we can configure it
                        // via the API.
                        .push_buffer("HTTP Forward", &config.http_request_buffer),
                )
            })
            .push_http_server()
            .into_inner();

        let opaque = self.push_tcp_endpoint().push_tcp_forward();

        opaque.push_detect_http(http).map_stack(|_, _, stk| {
            stk.instrument(|e: &tcp::Endpoint| info_span!("forward", endpoint = %e.addr))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::test_util::*;
    use hyper::{client::conn::Builder as ClientBuilder, Body, Request};
    use linkerd_app_core::{
        svc::{NewService, Service, ServiceExt},
        Error,
    };
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
                .push_forward()
                .into_inner()
                .new_service(tcp::Endpoint::forward(
                    OrigDstAddr(addr),
                    tls::NoClientTls::Disabled,
                    false,
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
