use super::Endpoint;
use crate::{
    http::{self, balance, breaker},
    metrics::BalancerMetricsParams,
    stack_labels, BackendRef, ParentRef,
};
use linkerd_app_core::{
    classify,
    config::QueueConfig,
    metrics::prom,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Error, NameAddr,
};
use linkerd_proxy_client_policy::FailureAccrual;
use std::{fmt::Debug, net::SocketAddr};
use tracing::info_span;

/// A target configuring a load balancer stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance<T> {
    pub addr: NameAddr,
    pub ewma: balance::EwmaConfig,
    pub queue: QueueConfig,
    pub parent: T,
}

/// Wraps errors encountered in this module.
#[derive(Debug, thiserror::Error)]
#[error("{}: {source}", backend.0)]
pub struct BalanceError {
    backend: BackendRef,
    #[source]
    source: Error,
}

// === impl Balance ===

impl<T> svc::Param<http::balance::EwmaConfig> for Balance<T> {
    fn param(&self) -> http::balance::EwmaConfig {
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

impl<T> Balance<T>
where
    // Parent target.
    T: svc::Param<ParentRef>,
    T: svc::Param<BackendRef>,
    T: svc::Param<FailureAccrual>,
    T: Clone + Debug + Send + Sync + 'static,
{
    pub(super) fn layer<N, NSvc, R>(
        config: &crate::Config,
        rt: &crate::Runtime,
        registry: &mut prom::registry::Registry,
        resolve: R,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneHttp<Self>> + Clone
    where
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata> + 'static,
        R::Resolution: Unpin,
        // Endpoint stack.
        N: svc::NewService<Endpoint<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        // TODO(ver) Configure queues from the target (i.e. from discovery).
        let http_queue = config.http_request_queue;
        let inbound_ips = config.inbound_ips.clone();
        let metrics = rt.metrics.clone();

        let resolve = svc::stack(resolve.into_service())
            .push_map_target(|t: Self| ConcreteAddr(t.addr))
            .into_inner();

        let metrics_params = BalancerMetricsParams::register(registry);

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
                            queue: http_queue,
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
                        let channel_capacity = http_queue.capacity;
                        move |target: &Self| breaker::Params {
                            accrual: target.parent.param(),
                            channel_capacity,
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
                ))
                .push_on_service(svc::OnServiceLayer::new(svc::BoxService::layer()))
                .push_on_service(svc::ArcNewService::layer())
                .push(svc::ArcNewService::layer());

            endpoint
                .push(http::NewBalancePeakEwma::layer(
                    resolve.clone(),
                    metrics_params.clone(),
                ))
                .push_on_service(http::BoxResponse::layer())
                .push_on_service(metrics.proxy.stack.layer(stack_labels("http", "balance")))
                .push(svc::NewMapErr::layer_from_target::<BalanceError, _>())
                .instrument(|t: &Self| {
                    let BackendRef(meta) = t.parent.param();
                    info_span!(
                        "service",
                        ns = %meta.namespace(),
                        name = %meta.name(),
                        port = %meta.port().map(u16::from).unwrap_or(0),
                    )
                })
                .arc_new_clone_http()
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
