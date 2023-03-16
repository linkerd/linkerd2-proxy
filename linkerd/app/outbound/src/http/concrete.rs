//! A stack that (optionally) resolves a service to a set of endpoint replicas
//! and distributes HTTP requests among them.

use super::{balance, client};
use crate::{http, stack_labels, Outbound};
use linkerd_app_core::{
    classify, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata, ProtocolHint},
        core::Resolve,
        http::classify::gate,
        tap,
    },
    svc::{self, Layer},
    tls,
    transport::{self, addrs::*},
    Error, Infallible, NameAddr,
};
use std::{fmt::Debug, net::SocketAddr, sync::Arc};
use tokio::{sync::Semaphore, time};
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

#[derive(Clone, Debug)]
struct ConsecutiveFailureGateSet;

#[derive(Copy, Clone, Debug)]
struct ConsecutiveFailureGate {
    max_failures: usize,
    channel_capacity: usize,
    init_ejection_backoff: time::Duration,
    max_ejection_backoff: time::Duration,
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

            let balance = endpoint
                .check_new_service::<Endpoint<T>, http::Request<http::BoxBody>>()
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
                        }
                    }
                })
                .check_new_service::<((SocketAddr, Metadata), Balance<T>), http::Request<http::BoxBody>>()
                .push_on_service(svc::MapErr::layer_boxed())
                .lift_new_with_target()
                .check_new_new_service::<Balance<T>, (SocketAddr, Metadata), http::Request<http::BoxBody>>()
                .push(
                    http::NewClassifyGateSet::<classify::Response, ConsecutiveFailureGate, _, _>::layer_via(
                        ConsecutiveFailureGateSet,
                    ),
                )
                .check_new_new_service::<Balance<T>, (SocketAddr, Metadata), http::Request<http::BoxBody>>()
                .push(http::NewBalancePeakEwma::layer(resolve))
                .check_new_service::<Balance<T>, http::Request<_>>()
                .push(svc::NewMapErr::layer_from_target::<ConcreteError, _>())
                .push_on_service(http::BoxResponse::layer())
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "balance")),
                )
                .instrument(|t: &Balance<T>| info_span!("balance", addr = %t.addr));

            balance
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Dispatch::Balance(addr, ewma) => {
                                svc::Either::A(Balance { addr, ewma, parent })
                            }
                            Dispatch::Forward(addr, metadata) => svc::Either::B({
                                let is_local = inbound_ips.contains(&addr.ip());
                                Endpoint {
                                    is_local,
                                    addr,
                                    metadata,
                                    parent,
                                }
                            }),
                        })
                    },
                    forward.into_inner(),
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

// === impl ConsecutiveFailureGateSet ===

impl<T> svc::ExtractParam<ConsecutiveFailureGate, Balance<T>> for ConsecutiveFailureGateSet {
    fn extract_param(&self, _: &Balance<T>) -> ConsecutiveFailureGate {
        ConsecutiveFailureGate {
            max_failures: 5,
            channel_capacity: 100,
            // TODO Use `ExponentialBackoff`
            init_ejection_backoff: time::Duration::from_secs(1),
            max_ejection_backoff: time::Duration::from_secs(60),
        }
    }
}

// === impl ConsecutiveFailureGate ===

impl<T> svc::ExtractParam<gate::Params<classify::Class>, T> for ConsecutiveFailureGate {
    fn extract_param(&self, _: &T) -> gate::Params<classify::Class> {
        let Self {
            max_failures,
            channel_capacity,
            init_ejection_backoff,
            max_ejection_backoff,
        } = *self;

        // Create a channel so that we can receive response summaries and
        // control the gate.
        let (prms, gate, mut rsps) = gate::Params::channel(channel_capacity);

        // 1. If 5 consecutive failures are encounted, shut the gate.
        // 2. After an ejection timeout, open the gate so that 1 request can be processed.
        // 3. If that request succeeds, open the gate. If it fails, increase the
        //    ejection timeout and repeat.
        enum State {
            Open { failures: usize },
            Probation(time::Duration),
            Shut(time::Duration),
        }
        tokio::spawn(async move {
            let mut state = State::Open { failures: 0 };
            let mut current_backoff = None;
            let mut sleep = Box::pin(tokio::time::sleep(time::Duration::from_secs(0)));

            loop {
                state = tokio::select! {
                    biased;
                    _ = gate.closed() => return,

                    // The transition future completes with a new state if we're
                    // in a Shut state waiting to enter probation.
                    _ = &mut sleep, if current_backoff.is_some() => {
                        State::Probation(current_backoff.take().unwrap())
                    }

                    rsp = rsps.recv() => match rsp {
                        None => return,
                        // Process a response summary, changing returning a new state if the state changes.
                        Some(class) => match state {
                            State::Open { ref mut failures } => match class {
                                classify::Class::Http(Ok(_)) | classify::Class::Grpc(Ok(_)) => {
                                    if *failures == 0 {
                                        continue;
                                    }
                                    State::Open { failures: 0 }
                                }
                                _ => {
                                    *failures += 1;
                                    if *failures < max_failures {
                                        continue;
                                    }
                                    State::Shut(init_ejection_backoff)
                                }
                            }
                            State::Shut(..) => continue,

                            State::Probation(backoff) => match class {
                                classify::Class::Http(Ok(_)) | classify::Class::Grpc(Ok(_)) => {
                                    State::Open { failures: 0 }
                                }
                                _ => State::Shut((backoff * 2).min(max_ejection_backoff)),
                            }
                        },
                    },
                };

                match state {
                    State::Open { .. } => {
                        current_backoff = None;
                        gate.open();
                    }

                    State::Shut(backoff) => {
                        sleep.as_mut().reset(time::Instant::now() + backoff);
                        current_backoff = Some(backoff);
                        gate.shut();
                    }

                    State::Probation(..) => {
                        current_backoff = None;
                        gate.limit(Arc::new(Semaphore::new(1)));
                    }
                }
            }
        });

        prms
    }
}
