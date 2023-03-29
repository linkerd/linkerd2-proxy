//! A stack that (optionally) resolves a service to a set of endpoint replicas
//! and distributes HTTP requests among them.

use super::{balance, client, handle_proxy_error_headers};
use crate::{http, stack_labels, Outbound};
use linkerd_app_core::{
    classify, metrics, profiles,
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
        T: svc::Param<Dispatch>,
        // TODO(ver) T: svc::Param<svc::queue::Capacity> + svc::Param<svc::queue::Timeout>,
        // Failure accrual policy.
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
        let resolve =
            svc::MapTargetLayer::new(|t: Balance<T>| -> ConcreteAddr { ConcreteAddr(t.addr) })
                .layer(resolve.into_service());

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

            let endpoint = inner
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "endpoint")),
                )
                .instrument(|e: &Endpoint<T>| info_span!("endpoint", addr = %e.addr));

            let mk_breaker = |target: &Balance<T>| {
                match target.parent.param() {
                    FailureAccrual::None => |_: &(SocketAddr, Metadata)| {
                        // Construct a gate channel, dropping the controller
                        // side of the channel such that response summaries
                        // are never read. The failure accrual gate never
                        // closes in this configuration.
                        tracing::trace!("No failure accrual policy enabled");
                        let (prms, _, _) = classify::gate::Params::channel(1);
                        prms
                    },
                }
            };

            let balance = endpoint
                .push_map_target({
                    let inbound_ips = inbound_ips.clone();
                    move |((addr, metadata), target): ((SocketAddr, Metadata), Balance<T>)| {
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
                    http::NewClassifyGateSet::<classify::Response, _, _, _>::layer_via(mk_breaker),
                )
                .push(http::NewBalancePeakEwma::layer(resolve))
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .push_on_service(http::BoxResponse::layer())
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "balance")),
                )
                .instrument(|t: &Balance<T>| info_span!("balance", addr = %t.addr));

            let fail = svc::ArcNewService::new(|message: Arc<str>| {
                svc::mk(move |_| {
                    let error = DispatcherFailed(message.clone());
                    futures::future::ready(Err(error))
                })
            });
            balance
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
                .push(svc::NewQueue::layer_via(config.http_request_queue))
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

impl<T> svc::Param<metrics::OutboundEndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> metrics::OutboundEndpointLabels {
        metrics::OutboundEndpointLabels {
            authority: self.parent.param(),
            labels: metrics::prefix_labels("dst", self.metadata.labels().iter()),
            server_id: self.param(),
            target_addr: self.addr.into(),
        }
    }
}

impl<T> svc::Param<metrics::EndpointLabels> for Endpoint<T>
where
    T: svc::Param<Option<http::uri::Authority>>,
{
    fn param(&self) -> metrics::EndpointLabels {
        metrics::EndpointLabels::Outbound(self.param())
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
        // FIXME(ver) create a dedicated extension type for route labels.
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}
