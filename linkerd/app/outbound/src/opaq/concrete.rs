use crate::{stack_labels, Outbound};
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
use std::{fmt::Debug, net::SocketAddr};
use tracing::info_span;

/// Parameter configuring dispatcher behavior.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dispatch {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(Remote<ServerAddr>, Metadata),
}

/// Wraps errors encountered in this module.
#[derive(Debug, thiserror::Error)]
#[error("concrete service {addr}: {source}")]
pub struct ConcreteError {
    addr: NameAddr,
    #[source]
    source: Error,
}

/// Inner stack target type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint<T> {
    addr: Remote<ServerAddr>,
    is_local: bool,
    metadata: Metadata,
    parent: T,
}

/// A target configuring a load balancer stack.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Balance<T> {
    addr: NameAddr,
    ewma: balance::EwmaConfig,
    parent: T,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a [`svc::NewService`] stack that builds buffered opaque services
    /// for `T`-typed concrete targets. Connections may be load balanced across
    /// a discovered set of replicas or forwarded to a single endpoint,
    /// depending on the value of the `Dispatch` parameter.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    pub fn push_opaq_concrete<T, I, R>(
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
        T: svc::Param<Dispatch>,
        T: Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        // Endpoint connector.
        C: svc::MakeConnection<Endpoint<T>> + Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
        C: Send + Sync + 'static,
    {
        let resolve =
            svc::MapTargetLayer::new(|t: Balance<T>| -> ConcreteAddr { ConcreteAddr(t.addr) })
                .layer(resolve.into_service());

        self.map_stack(|config, rt, inner| {
            let crate::Config {
                tcp_connection_queue,
                ..
            } = config;

            let connect = inner
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_new_thunk();

            let forward = connect
                .clone()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("opaq", "forward")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("forward", addr = %e.addr));

            let endpoint = connect
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("opaq", "endpoint")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("endpoint", addr = %e.addr));

            let inbound_ips = config.inbound_ips.clone();
            let balance = endpoint
                .push_map_target(
                    move |((addr, metadata), target): ((SocketAddr, Metadata), Balance<T>)| {
                        tracing::trace!(%addr, ?metadata, ?target, "Resolved endpoint");
                        let is_local = inbound_ips.contains(&addr.ip());
                        Endpoint {
                            addr: Remote(ServerAddr(addr)),
                            metadata,
                            is_local,
                            parent: target.parent,
                        }
                    },
                )
                .lift_new_with_target()
                .push(tcp::NewBalancePeakEwma::layer(resolve))
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("opaq", "balance")),
                )
                .instrument(|t: &Balance<T>| info_span!("balance", addr = %t.addr));

            balance
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Dispatch::Balance(addr, ewma) => {
                                svc::Either::A(Balance { addr, ewma, parent })
                            }
                            Dispatch::Forward(addr, meta) => svc::Either::B(Endpoint {
                                addr,
                                is_local: false,
                                metadata: meta,
                                parent,
                            }),
                        })
                    },
                    forward.into_inner(),
                )
                .push_on_service(tcp::Forward::layer())
                .push_on_service(drain::Retain::layer(rt.drain.clone()))
                .push(svc::NewQueue::layer_via(*tcp_connection_queue))
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

impl<T> std::ops::Deref for Balance<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

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

impl<T> svc::Param<Option<crate::tcp::tagged_transport::PortOverride>> for Endpoint<T> {
    fn param(&self) -> Option<crate::tcp::tagged_transport::PortOverride> {
        if self.is_local {
            return None;
        }
        self.metadata
            .tagged_transport_port()
            .map(crate::tcp::tagged_transport::PortOverride)
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
        metrics::OutboundEndpointLabels {
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.param(),
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
        let use_transport_header = self.metadata.tagged_transport_port().is_some()
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
