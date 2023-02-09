use super::{balance, client, normalize_uri};
use crate::{http, stack_labels, Outbound};
use ahash::AHashSet;
use linkerd_app_core::{
    metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata, ProtocolHint},
        core::Resolve,
        tap,
    },
    svc::{self, Layer},
    tls,
    transport::{self, addrs::*},
    Error, Infallible, NameAddr,
};
use std::{
    fmt::Debug,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(Remote<ServerAddr>, Metadata),
}

#[derive(Debug, thiserror::Error)]
#[error("concrete service {addr}: {source}")]
pub struct ConcreteError {
    addr: NameAddr,
    #[source]
    source: Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint<T> {
    addr: Remote<ServerAddr>,
    is_local: bool,
    metadata: Metadata,
    parent: T,
}

#[derive(Clone, Debug)]
pub struct NewEndpoint<N> {
    inner: N,
    inbound_ips: IpSet,
}

pub type IpSet = Arc<AHashSet<IpAddr>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Balance<T> {
    addr: NameAddr,
    ewma: balance::EwmaConfig,
    parent: T,
}

impl<N> Outbound<N> {
    /// Builds a [`svc::NewService`] stack that builds buffered HTTP load
    /// balancer services for [`Concrete`] targets.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    //
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_http_concrete<T, NSvc, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        T: svc::Param<Target>,
        // T: svc::Param<svc::queue::Capacity>,
        // T: svc::Param<svc::queue::Timeout>,
        T: Clone + Debug + Send + Sync + 'static,
        N: svc::NewService<Endpoint<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata>,
    {
        let resolve =
            svc::MapTargetLayer::new(|t: Balance<T>| -> ConcreteAddr { ConcreteAddr(t.addr) })
                .layer(resolve.into_service());

        self.map_stack(|config, rt, endpoint| {
            let inbound_ips = config.inbound_ips.clone();

            let endpoint = endpoint
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "endpoint")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("endpoint", addr = %e.addr))
                .check_new_service::<Endpoint<T>, http::Request<_>>();

            endpoint
                .clone()
                .push(NewEndpoint::layer(inbound_ips.iter().copied()))
                .lift_new_with_target()
                .check_new_new_service::<Balance<T>, (_, _), http::Request<_>>()
                .push(http::NewBalancePeakEwma::layer(resolve))
                .check_new_service::<Balance<T>, http::Request<_>>()
                // Drives the initial resolution via the service's readiness.
                .push_on_service(
                    svc::layers().push(http::BoxResponse::layer()).push(
                        rt.metrics
                            .proxy
                            .stack
                            .layer(stack_labels("http", "concrete")),
                    ),
                )
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .instrument(|t: &Balance<T>| info_span!("concrete", addr = %t.addr))
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Target::Balance(addr, ewma) => {
                                svc::Either::A(Balance { addr, ewma, parent })
                            }
                            Target::Forward(addr, meta) => svc::Either::B(Endpoint {
                                addr,
                                is_local: false,
                                metadata: meta,
                                parent,
                            }),
                        })
                    },
                    endpoint.into_inner(),
                )
                .push(svc::NewQueue::layer_via(config.http_request_queue))
                .check_new_service::<T, http::Request<_>>()
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ConcreteError ===

impl<T> From<(&Balance<T>, Error)> for ConcreteError {
    fn from((target, source): (&Balance<T>, Error)) -> Self {
        Self {
            addr: target.addr.clone(),
            source,
        }
    }
}

// === impl Balance ===

impl<T> svc::Param<http::balance::EwmaConfig> for Balance<T> {
    fn param(&self) -> http::balance::EwmaConfig {
        self.ewma
    }
}

// === impl Endpoint ===

impl<T> svc::Param<Remote<ServerAddr>> for Endpoint<T> {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl<T> svc::Param<Option<crate::tcp::opaque_transport::PortOverride>> for Endpoint<T> {
    fn param(&self) -> Option<crate::tcp::opaque_transport::PortOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .opaque_transport_port()
            .map(crate::tcp::opaque_transport::PortOverride)
    }
}

impl<T> svc::Param<Option<http::AuthorityOverride>> for Endpoint<T> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .authority_override()
            .cloned()
            .map(http::AuthorityOverride)
    }
}

impl<T> svc::Param<transport::labels::Key> for Endpoint<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl<T> svc::Param<metrics::OutboundEndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = self
            .parent
            .param()
            .as_ref()
            .map(|profiles::LogicalAddr(a)| a.as_http_authority());
        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.param(),
            target_addr: self.addr.into(),
        }
    }
}

impl<T> svc::Param<metrics::EndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> metrics::EndpointLabels {
        metrics::EndpointLabels::from(svc::Param::<metrics::OutboundEndpointLabels>::param(self))
    }
}

impl<T> svc::Param<tls::ConditionalClientTls> for Endpoint<T> {
    fn param(&self) -> tls::ConditionalClientTls {
        if self.is_local {
            return tls::ConditionalClientTls::None(tls::NoClientTls::Loopback);
        }

        // If we're transporting an opaque protocol OR we're communicating with
        // a gateway, then set an ALPN value indicating support for a transport
        // header.
        let use_transport_header = self.metadata.opaque_transport_port().is_some()
            || self.metadata.authority_override().is_some();
        self.metadata
            .identity()
            .cloned()
            .map(move |server_id| {
                tls::ConditionalClientTls::Some(tls::ClientTls {
                    server_id,
                    alpn: if use_transport_header {
                        use linkerd_app_core::transport_header::PROTOCOL;
                        Some(tls::client::AlpnProtocols(vec![PROTOCOL.into()]))
                    } else {
                        None
                    },
                })
            })
            .unwrap_or(tls::ConditionalClientTls::None(
                tls::NoClientTls::NotProvidedByServiceDiscovery,
            ))
    }
}

impl<T> svc::Param<normalize_uri::DefaultAuthority> for Endpoint<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> normalize_uri::DefaultAuthority {
        if let Some(profiles::LogicalAddr(ref a)) = self.parent.param() {
            return normalize_uri::DefaultAuthority(Some(
                a.to_string()
                    .parse()
                    .expect("Address must be a valid authority"),
            ));
        }

        normalize_uri::DefaultAuthority(Some(
            self.addr
                .to_string()
                .parse()
                .expect("Address must be a valid authority"),
        ))
    }
}

impl<T> svc::Param<http::Version> for Endpoint<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> http::Version {
        self.parent.param()
    }
}

impl<T> svc::Param<client::Settings> for Endpoint<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> client::Settings {
        match self.param() {
            http::Version::H2 => client::Settings::H2,
            http::Version::Http1 => match self.metadata.protocol_hint() {
                ProtocolHint::Unknown => client::Settings::Http1,
                ProtocolHint::Http2 => client::Settings::OrigProtoUpgrade,
            },
        }
    }
}

// TODO(ver) move this into the endpoint stack?
impl<T> tap::Inspect for Endpoint<T> {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<http::ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::None(tls::NoServerTls::Loopback)
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr.into())
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        svc::Param::<tls::ConditionalClientTls>::param(self)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<tap::Labels> {
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

/// === impl NewEndpoint ===

impl<N> NewEndpoint<N> {
    pub fn new(inbound_ips: IpSet, inner: N) -> Self {
        Self { inbound_ips, inner }
    }

    pub fn layer(
        inbound_ips: impl IntoIterator<Item = IpAddr>,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        let inbound_ips = Arc::new(inbound_ips.into_iter().collect::<AHashSet<_>>());
        svc::layer::mk(move |inner| Self::new(inbound_ips.clone(), inner))
    }
}

impl<T, N> svc::NewService<((SocketAddr, Metadata), Balance<T>)> for NewEndpoint<N>
where
    T: Clone + Debug,
    N: svc::NewService<Endpoint<T>>,
{
    type Service = N::Service;

    fn new_service(
        &self,
        ((addr, metadata), parent): ((SocketAddr, Metadata), Balance<T>),
    ) -> Self::Service {
        tracing::trace!(%addr, ?metadata, ?parent, "Resolved endpoint");
        let is_local = self.inbound_ips.contains(&addr.ip());
        self.inner.new_service(Endpoint {
            addr: Remote(ServerAddr(addr)),
            metadata,
            is_local,
            parent: parent.parent,
        })
    }
}
