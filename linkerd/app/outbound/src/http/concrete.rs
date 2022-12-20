use super::{Concrete, Endpoint};
use crate::{endpoint, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
        resolve::map_endpoint,
    },
    svc, Error, Infallible,
};
use tracing::debug_span;

impl<N> Outbound<N> {
    pub fn push_http_concrete<NSvc, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Concrete,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
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
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata>,
        R: Clone + Send + Sync + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, endpoint| {
            let crate::Config {
                http_concrete_buffer,
                orig_dst_idle_timeout,
                ..
            } = config;

            let resolve = svc::stack(resolve.into_service())
                .check_service::<ConcreteAddr>()
                .push_request_filter(|c: Concrete| Ok::<_, Infallible>(c.resolve))
                .push(svc::layer::mk(move |inner| {
                    map_endpoint::Resolve::new(
                        endpoint::FromMetadata {
                            inbound_ips: config.inbound_ips.clone(),
                        },
                        inner,
                    )
                }))
                .check_service::<Concrete>()
                .into_inner();

            endpoint
                .instrument(|e: &Endpoint| debug_span!("endpoint", server.addr = %e.addr))
                .check_new_service::<Endpoint, http::Request<http::BoxBody>>()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("http", "concrete.endpoint")),
                )
                .check_new_service::<Endpoint, http::Request<http::BoxBody>>()
                // Resolve the service to its endpoints and balance requests over them.
                //
                // We *don't* ensure that the endpoint is driven to readiness here, because this
                // might cause us to continually attempt to reestablish connections without
                // consulting discovery to see whether the endpoint has been removed. Instead, the
                // endpoint stack spawns each _connection_ attempt on a background task, but the
                // decision to attempt the connection must be driven by the balancer.
                //
                // TODO(ver) remove the watchdog timeout.
                .push(resolve::layer(resolve, *orig_dst_idle_timeout * 2))
                .push_on_service(http::balance::layer(
                    crate::EWMA_DEFAULT_RTT,
                    crate::EWMA_DECAY,
                ))
                .check_make_service::<Concrete, http::Request<http::BoxBody>>()
                .push(svc::MapErr::layer(Into::into))
                // Drives the initial resolution via the service's readiness.
                .into_new_service()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxResponse::layer())
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "concrete")),
                        )
                        .push_buffer::<http::Request<http::BoxBody>>(
                            "HTTP Concrete",
                            http_concrete_buffer.capacity,
                            http_concrete_buffer.failfast_timeout,
                        ),
                )
                .instrument(|c: &Concrete| debug_span!("concrete", addr = %c.resolve))
                .push(svc::ArcNewService::layer())
        })
    }
}
