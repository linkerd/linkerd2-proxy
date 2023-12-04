//! A stack that (optionally) resolves a service to a set of endpoint replicas
//! and distributes HTTP requests among them.

use super::{balance, breaker, client, handle_proxy_error_headers};
use crate::{http, stack_labels, BackendRef, Outbound, ParentRef};
use linkerd_app_core::{
    classify,
    metrics::{prefix_outbound_endpoint_labels, EndpointLabels, OutboundEndpointLabels},
    profiles,
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
use linkerd_proxy_client_policy::FailureAccrual;
use std::{fmt::Debug, net::SocketAddr, sync::Arc};
use tracing::info_span;

mod metrics;
#[cfg(test)]
mod tests;

pub use self::metrics::BalancerMetrics;

/// Parameter configuring dispatcher behavior.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dispatch {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(Remote<ServerAddr>, Metadata),
    Fail { message: Arc<str> },
}

/// A backend dispatcher explicitly fails all requests.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DispatcherFailed(Arc<str>);

/// Wraps errors encountered in this module.
#[derive(Debug, thiserror::Error)]
#[error("{}: {source}", backend.0)]
pub struct BalanceError {
    backend: BackendRef,
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
    close_server_connection_on_remote_proxy_error: bool,
}

/// A target configuring a load balancer stack.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Balance<T> {
    addr: NameAddr,
    ewma: balance::EwmaConfig,
    parent: T,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a [`svc::NewService`] stack that builds buffered HTTP services
    /// for `T`-typed concrete targets. Requests may be load balanced across a
    /// discovered set of replicas or forwarded to a single endpoint, depending
    /// on the value of the `Dispatch` parameter.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
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
        // Concrete target type.
        T: svc::Param<ParentRef>,
        T: svc::Param<BackendRef>,
        T: svc::Param<Dispatch>,
        T: svc::Param<FailureAccrual>,
        T: Clone + Debug + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata>,
        // Endpoint stack.
        N: svc::NewService<Endpoint<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, inner| {
            let inbound_ips = config.inbound_ips.clone();

            let forward = inner
                .clone()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "forward")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("forward", addr = %e.addr));

            let fail = svc::ArcNewService::new(|message: Arc<str>| {
                svc::mk(move |_| {
                    let error = DispatcherFailed(message.clone());
                    futures::future::ready(Err(error))
                })
            });

            inner
                .push(Balance::layer(config, rt, resolve))
                .push_switch(Ok::<_, Infallible>, forward.into_inner())
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Dispatch::Balance(addr, ewma) => {
                                svc::Either::A(svc::Either::A(Balance { addr, ewma, parent }))
                            }
                            Dispatch::Forward(addr, metadata) => svc::Either::A(svc::Either::B({
                                let is_local = inbound_ips.contains(&addr.ip());
                                Endpoint {
                                    is_local,
                                    addr,
                                    metadata,
                                    parent,
                                    close_server_connection_on_remote_proxy_error: true,
                                }
                            })),
                            Dispatch::Fail { message } => svc::Either::B(message),
                        })
                    },
                    fail,
                )
                // TODO(ver) Configure this queue from the target (i.e. from
                // discovery).
                .push(svc::NewQueue::layer_via(config.http_request_queue))
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Balance ===

impl<T> svc::Param<http::balance::EwmaConfig> for Balance<T> {
    fn param(&self) -> http::balance::EwmaConfig {
        self.ewma
    }
}

impl<T> Balance<T>
where
    // Parent target.
    T: svc::Param<ParentRef>,
    T: svc::Param<BackendRef>,
    T: svc::Param<FailureAccrual>,
    T: Clone + Debug + Send + Sync + 'static,
{
    pub fn layer<N, NSvc, R>(
        config: &crate::Config,
        rt: &crate::Runtime,
        resolve: R,
    ) -> impl svc::Layer<
        N,
        Service = svc::ArcNewService<
            Self,
            impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = BalanceError,
                Future = impl std::future::Future<
                    Output = Result<http::Response<http::BoxBody>, BalanceError>,
                > + Send,
            >,
        >,
    > + Clone
    where
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata> + 'static,
        // Endpoint stack.
        N: svc::NewService<Endpoint<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        let classify_channel_capacity = config.http_request_queue.capacity;
        let inbound_ips = config.inbound_ips.clone();
        let metrics = rt.metrics.clone();

        let resolve = svc::MapTargetLayer::new(|t: Self| -> ConcreteAddr { ConcreteAddr(t.addr) })
            .layer(resolve.into_service());

        svc::layer::mk(move |inner: N| {
            let endpoint = svc::stack(inner)
                .push_map_target({
                    let inbound_ips = inbound_ips.clone();
                    move |((addr, metadata), target): ((SocketAddr, Metadata), Self)| {
                        tracing::trace!(%addr, ?metadata, ?target, "Resolved endpoint");
                        let is_local = inbound_ips.contains(&addr.ip());
                        Endpoint {
                            addr: Remote(ServerAddr(addr)),
                            metadata,
                            is_local,
                            parent: target.parent,
                            // We don't close server-side connections when we
                            // get `l5d-proxy-connection: close` response headers
                            // going through the balancer.
                            close_server_connection_on_remote_proxy_error: false,
                        }
                    }
                })
                .push_on_service(svc::MapErr::layer_boxed())
                .lift_new_with_target()
                .push(
                    http::NewClassifyGateSet::<classify::Response, _, _, _>::layer_via({
                        move |target: &Self| breaker::Params {
                            accrual: target.parent.param(),
                            // TODO configure channel capacities from target.
                            channel_capacity: classify_channel_capacity,
                        }
                    }),
                )
                .push(balance::NewGaugeEndpoints::layer_via({
                    let metrics = metrics.http_balancer.clone();
                    move |target: &Self| {
                        metrics.http_endpoints(target.parent.param(), target.parent.param())
                    }
                }))
                .push_on_service(svc::OnServiceLayer::new(
                    metrics.proxy.stack.layer(stack_labels("http", "endpoint")),
                ))
                .push_on_service(svc::NewInstrumentLayer::new(
                    |(addr, _): &(SocketAddr, _)| info_span!("endpoint", %addr),
                ));

            endpoint
                .push(http::NewBalancePeakEwma::layer(resolve.clone()))
                .push(svc::NewMapErr::layer_from_target::<BalanceError, _>())
                .push_on_service(http::BoxResponse::layer())
                .push_on_service(metrics.proxy.stack.layer(stack_labels("http", "balance")))
                .instrument(|t: &Self| {
                    let BackendRef(meta) = t.parent.param();
                    info_span!(
                        "service",
                        ns = %meta.namespace(),
                        name = %meta.name(),
                        port = %meta.port().map(u16::from).unwrap_or(0),
                    )
                })
                .push(svc::ArcNewService::layer())
                .into_inner()
        })
    }
}

// === impl BalanceError ===

impl<T> From<(&Balance<T>, Error)> for BalanceError
where
    T: svc::Param<BackendRef>,
{
    fn from((target, source): (&Balance<T>, Error)) -> Self {
        let backend = target.parent.param();
        Self { backend, source }
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

impl<T> svc::Param<handle_proxy_error_headers::CloseServerConnection> for Endpoint<T> {
    fn param(&self) -> handle_proxy_error_headers::CloseServerConnection {
        handle_proxy_error_headers::CloseServerConnection(
            self.close_server_connection_on_remote_proxy_error,
        )
    }
}

impl<T> svc::Param<transport::labels::Key> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl<T> svc::Param<OutboundEndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> OutboundEndpointLabels {
        OutboundEndpointLabels {
            labels: prefix_outbound_endpoint_labels("dst", self.metadata.labels().iter()),
            server_id: self.param(),
        }
    }
}

impl<T> svc::Param<EndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> EndpointLabels {
        EndpointLabels::Outbound(self.param())
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
            http::Version::Http1 => {
                // When the target is local (i.e. same as source of traffic)
                // then do not perform a protocol upgrade to HTTP/2
                if self.is_local {
                    return client::Settings::Http1;
                }
                match self.metadata.protocol_hint() {
                    // If the protocol hint is unknown or indicates that the
                    // endpoint's proxy will treat connections as opaque, do not
                    // perform a protocol upgrade to HTTP/2.
                    ProtocolHint::Unknown | ProtocolHint::Opaque => client::Settings::Http1,
                    ProtocolHint::Http2 => client::Settings::OrigProtoUpgrade,
                }
            }
        }
    }
}

impl<T> svc::Param<ProtocolHint> for Endpoint<T> {
    fn param(&self) -> ProtocolHint {
        self.metadata.protocol_hint()
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
        // FIXME(ver) create a dedicated extension type for route labels.
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}
