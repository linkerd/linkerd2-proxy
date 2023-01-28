use super::{Concrete, Endpoint};
use crate::{endpoint, stack_labels, Outbound};
use linkerd_app_core::{
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
    },
    svc, Error,
};
use std::time;
use tracing::info_span;

#[derive(Debug, thiserror::Error)]
#[error("concrete service {addr}: {source}")]
pub struct ConcreteError {
    addr: ConcreteAddr,
    #[source]
    source: Error,
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
    pub fn push_http_concrete<NSvc, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Concrete,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = ConcreteError,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        N: svc::NewService<Endpoint, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        R: Clone + Send + Sync + 'static,
        R: Resolve<Concrete, Error = Error, Endpoint = Metadata>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, endpoint| {
            endpoint
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "endpoint")),
                )
                .instrument(|e: &Endpoint| info_span!("endpoint", addr = %e.addr))
                .lift_new()
                .push(endpoint::NewFromMetadata::layer(config.inbound_ips.clone()))
                .push(http::NewBalancePeakEwma::layer(resolve))
                // Drives the initial resolution via the service's readiness.
                .push_on_service(
                    svc::layers().push(http::BoxResponse::layer()).push(
                        rt.metrics
                            .proxy
                            .stack
                            .layer(stack_labels("http", "concrete")),
                    ),
                )
                .push(svc::NewQueue::layer_with_timeout_via(
                    config.http_request_queue,
                ))
                .instrument(|c: &Concrete| info_span!("concrete", svc = %c.resolve))
                .push(svc::NewMapErr::layer_from_target())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Concrete ===

impl svc::Param<http::balance::EwmaConfig> for Concrete {
    fn param(&self) -> http::balance::EwmaConfig {
        http::balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        }
    }
}

// === impl ConcreteError ===

impl<T: svc::Param<ConcreteAddr>> From<(&T, Error)> for ConcreteError {
    fn from((target, source): (&T, Error)) -> Self {
        Self {
            addr: target.param(),
            source,
        }
    }
}
