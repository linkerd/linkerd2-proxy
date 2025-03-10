use super::Logical;
use crate::{
    metrics::BalancerMetricsParams,
    stack_labels,
    zone::{tcp_zone_labels, TcpZoneLabels},
    BackendRef, Outbound, ParentRef,
};
use linkerd_app_core::{
    config::QueueConfig,
    drain, io,
    metrics::{
        self,
        prom::{self, EncodeLabelSetMut},
        OutboundZoneLocality,
    },
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
use std::{fmt::Debug, net::SocketAddr, sync::Arc};
use tracing::info_span;

/// Parameter configuring dispatcher behavior.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dispatch {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(Remote<ServerAddr>, Metadata),
    /// A backend dispatcher that explicitly fails all requests.
    Fail {
        message: Arc<str>,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DispatcherFailed(Arc<str>);

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

pub type BalancerMetrics = BalancerMetricsParams<ConcreteLabels>;

/// A target configuring a load balancer stack.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Balance<T> {
    addr: NameAddr,
    ewma: balance::EwmaConfig,
    queue: QueueConfig,
    parent: T,
}

// TODO: Use crate::metrics::ConcreteLabels once we do not need the logical and concrete labels anymore
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ConcreteLabels {
    parent: ParentRef,
    backend: BackendRef,
    logical: Arc<str>,
    concrete: Arc<str>,
}

impl prom::EncodeLabelSetMut for ConcreteLabels {
    fn encode_label_set(&self, enc: &mut prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        use prom::encoding::EncodeLabel;
        self.parent.encode_label_set(enc)?;
        self.backend.encode_label_set(enc)?;
        ("logical", &*self.logical).encode(enc.encode_label())?;
        ("concrete", &*self.concrete).encode(enc.encode_label())?;
        Ok(())
    }
}

impl prom::encoding::EncodeLabelSet for ConcreteLabels {
    fn encode(&self, mut enc: prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

impl<T> svc::ExtractParam<balance::Metrics, Balance<T>> for BalancerMetricsParams<ConcreteLabels>
where
    T: svc::Param<Logical>,
    T: svc::Param<BackendRef>,
{
    fn extract_param(&self, bal: &Balance<T>) -> balance::Metrics {
        let Logical { addr, meta: parent } = bal.parent.param();
        self.metrics(&ConcreteLabels {
            parent,
            logical: addr.to_string().into(),
            backend: bal.parent.param(),
            concrete: bal.addr.to_string().into(),
        })
    }
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
        // Logical target
        T: svc::Param<Logical>,
        T: svc::Param<Dispatch>,
        T: Clone + Debug + Send + Sync + 'static,
        T: svc::Param<BackendRef>,
        T: svc::Param<ParentRef>,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
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
            let queue = config.tcp_connection_queue;

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

            let fail = svc::ArcNewService::new(|message: Arc<str>| {
                svc::mk(move |_| futures::future::ready(Err(DispatcherFailed(message.clone()))))
            });

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
                .push(tcp::NewBalance::layer(
                    resolve,
                    rt.metrics.prom.opaq.balance.clone(),
                ))
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("opaq", "balance")),
                )
                .instrument(|t: &Balance<T>| info_span!("balance", addr = %t.addr));

            balance
                .push_switch(Ok::<_, Infallible>, forward.into_inner())
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Dispatch::Balance(addr, ewma) => {
                                svc::Either::Left(svc::Either::Left(Balance {
                                    addr,
                                    ewma,
                                    queue,
                                    parent,
                                }))
                            }

                            Dispatch::Forward(addr, meta) => {
                                svc::Either::Left(svc::Either::Right(Endpoint {
                                    addr,
                                    is_local: false,
                                    metadata: meta,
                                    parent,
                                }))
                            }
                            Dispatch::Fail { message } => svc::Either::Right(message),
                        })
                    },
                    svc::stack(fail).check_new_clone().into_inner(),
                )
                .push_on_service(tcp::Forward::layer())
                .push_on_service(drain::Retain::layer(rt.drain.clone()))
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

impl<T> svc::Param<svc::queue::Capacity> for Balance<T> {
    fn param(&self) -> svc::queue::Capacity {
        svc::queue::Capacity(self.queue.capacity)
    }
}

impl<T> svc::Param<svc::queue::Timeout> for Balance<T> {
    fn param(&self) -> svc::queue::Timeout {
        svc::queue::Timeout(self.queue.failfast_timeout)
    }
}

impl<T: svc::Param<ParentRef>> svc::Param<ParentRef> for Balance<T> {
    fn param(&self) -> ParentRef {
        self.parent.param()
    }
}

impl<T: svc::Param<BackendRef>> svc::Param<BackendRef> for Balance<T> {
    fn param(&self) -> BackendRef {
        self.parent.param()
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
    T: svc::Param<Logical>,
{
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl<T> svc::Param<metrics::OutboundEndpointLabels> for Endpoint<T>
where
    T: svc::Param<Logical>,
{
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = self
            .parent
            .param()
            .addr
            .name_addr()
            .map(|a| a.as_http_authority());

        metrics::OutboundEndpointLabels {
            authority,
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            zone_locality: self.param(),
            server_id: self.param(),
            target_addr: self.addr.into(),
        }
    }
}

impl<T> svc::Param<OutboundZoneLocality> for Endpoint<T> {
    fn param(&self) -> OutboundZoneLocality {
        OutboundZoneLocality::new(&self.metadata)
    }
}

impl<T> svc::Param<TcpZoneLabels> for Endpoint<T> {
    fn param(&self) -> TcpZoneLabels {
        tcp_zone_labels(self.param())
    }
}

impl<T> svc::Param<metrics::EndpointLabels> for Endpoint<T>
where
    T: svc::Param<Logical>,
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
            .map(move |mut client_tls| {
                client_tls.alpn = if use_transport_header {
                    use linkerd_app_core::transport_header::PROTOCOL;
                    Some(tls::client::AlpnProtocols(vec![PROTOCOL.into()]))
                } else {
                    None
                };

                tls::ConditionalClientTls::Some(client_tls)
            })
            .unwrap_or(tls::ConditionalClientTls::None(
                tls::NoClientTls::NotProvidedByServiceDiscovery,
            ))
    }
}
