use crate::{stack_labels, Outbound};
use ahash::AHashSet;
use linkerd_app_core::{
    drain, io, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http::AuthorityOverride,
        tcp::{self, balance},
    },
    svc::{self, layer::Layer},
    tls,
    transport::{self, addrs::*},
    transport_header::SessionProtocol,
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
struct NewEndpoint<N> {
    inner: N,
    inbound_ips: IpSet,
}

type IpSet = Arc<AHashSet<IpAddr>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Balance<T> {
    addr: NameAddr,
    ewma: balance::EwmaConfig,
    parent: T,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a [`svc::NewService`] stack that builds buffered opaque TCP load
    /// balancer services for [`Concrete`] targets.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    //
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_opaque_concrete<T, I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        // Logical target.c
        T: svc::Param<Target>,
        T: Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // #ndpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        // Endpoint stack.
        C: svc::MakeConnection<Endpoint<T>> + Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
        C: Send + Sync + 'static,
    {
        let resolve =
            svc::MapTargetLayer::new(|t: Balance<T>| -> ConcreteAddr { ConcreteAddr(t.addr) })
                .layer(resolve.into_service());

        self.map_stack(|config, rt, connect| {
            let crate::Config {
                tcp_connection_queue,
                ..
            } = config;

            let endpoint = connect
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_new_thunk()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("tcp", "endpoint")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("endpoint", addr = %e.addr));

            endpoint
                .clone()
                .check_new_service::<Endpoint<T>, ()>()
                .push(NewEndpoint::layer(config.inbound_ips.iter().copied()))
                .lift_new_with_target()
                .check_new_new_service::<Balance<T>, (_, _), ()>()
                .push(tcp::NewBalancePeakEwma::layer(resolve))
                .check_new_service::<Balance<T>, ()>()
                .push_on_service(
                    svc::layers().push(
                        rt.metrics
                            .proxy
                            .stack
                            .layer(stack_labels("opaque", "concrete")),
                    ),
                )
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .instrument(|t: &Balance<T>| info_span!("concrete", addr = %t.addr))
                .check_new_service::<Balance<T>, ()>()
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
                    endpoint.check_new_service::<Endpoint<T>, ()>().into_inner(),
                )
                .push_on_service(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .push(svc::NewQueue::layer_via(*tcp_connection_queue))
                .check_new_service::<T, I>()
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

impl<T> svc::Param<balance::EwmaConfig> for Balance<T> {
    fn param(&self) -> balance::EwmaConfig {
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

impl<T> svc::Param<Option<AuthorityOverride>> for Endpoint<T> {
    fn param(&self) -> Option<AuthorityOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .authority_override()
            .cloned()
            .map(AuthorityOverride)
    }
}

impl<T> svc::Param<Option<SessionProtocol>> for Endpoint<T> {
    fn param(&self) -> Option<SessionProtocol> {
        None
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

/// === impl NewEndpoint ===

impl<N> NewEndpoint<N> {
    pub fn new(inbound_ips: IpSet, inner: N) -> Self {
        Self { inbound_ips, inner }
    }

    pub fn layer(
        inbound_ips: impl IntoIterator<Item = IpAddr>,
    ) -> impl svc::Layer<N, Service = Self> + Clone {
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
        ((addr, metadata), target): ((SocketAddr, Metadata), Balance<T>),
    ) -> Self::Service {
        tracing::trace!(%addr, ?metadata, ?target, "Resolved endpoint");
        let is_local = self.inbound_ips.contains(&addr.ip());
        self.inner.new_service(Endpoint {
            addr: Remote(ServerAddr(addr)),
            metadata,
            is_local,
            parent: target.parent,
        })
    }
}
