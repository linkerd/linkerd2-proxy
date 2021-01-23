use super::{Concrete, Endpoint, Logical};
use crate::{resolve, stack_labels};
use linkerd_app_core::{
    classify,
    config::ProxyConfig,
    metrics, profiles,
    proxy::{api_resolve::Metadata, core::Resolve, http},
    retry, svc, tls, Addr, Error, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
};
use tracing::debug_span;

pub fn stack<B, E, ESvc, R>(
    config: &ProxyConfig,
    endpoint: E,
    resolve: R,
    metrics: metrics::Proxy,
) -> impl svc::NewService<
    Logical,
    Service = impl svc::Service<
        http::Request<B>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
        Future = impl Send,
    >,
> + Clone
where
    B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
    B::Data: Send + 'static,
    E: svc::NewService<Endpoint, Service = ESvc> + Clone + Send + 'static,
    ESvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    ESvc::Error: Into<Error>,
    ESvc::Future: Send,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send,
{
    let ProxyConfig {
        buffer_capacity,
        cache_max_idle_age,
        dispatch_timeout,
        ..
    } = config.clone();
    let watchdog = cache_max_idle_age * 2;

    svc::stack(endpoint.clone())
        .push_on_response(
            svc::layers()
                .push(http::BoxRequest::layer())
                .push(
                    metrics
                        .stack
                        .layer(stack_labels("http", "balance.endpoint")),
                )
                // Ensure individual endpoints are driven to readiness so that
                // the balancer need not drive them all directly.
                .push(svc::layer::mk(svc::SpawnReady::new)),
        )
        // Resolve the service to its endponts and balance requests over them.
        //
        // If the balancer has been empty/unavailable, eagerly fail requests.
        // When the balancer is in failfast, spawn the service in a background
        // task so it becomes ready without new requests.
        .push(resolve::layer(resolve, watchdog))
        .push_on_response(
            svc::layers()
                .push(http::balance::layer(
                    crate::EWMA_DEFAULT_RTT,
                    crate::EWMA_DECAY,
                ))
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("HTTP Balancer", dispatch_timeout))
                .push(metrics.stack.layer(stack_labels("http", "concrete"))),
        )
        .push(svc::MapErrLayer::new(Into::into))
        // Drives the initial resolution via the service's readiness.
        .into_new_service()
        // The concrete address is only set when the profile could be
        // resolved. Endpoint resolution is skipped when there is no
        // concrete address.
        .instrument(|c: &Concrete| match c.resolve.as_ref() {
            None => debug_span!("concrete"),
            Some(addr) => debug_span!("concrete", %addr),
        })
        .push_map_target(Concrete::from)
        // Distribute requests over a distribution of balancers via a traffic
        // split.
        //
        // If the traffic split is empty/unavailable, eagerly fail
        // requests requests. When the split is in failfast, spawn
        // the service in a background task so it becomes ready without
        // new requests.
        .push(profiles::split::layer())
        .push_on_response(
            svc::layers()
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity),
        )
        // Note: routes can't exert backpressure.
        .push(profiles::http::route_request::layer(
            svc::proxies()
                .push(
                    metrics
                        .http_route_actual
                        .to_layer::<classify::Response, _>(),
                )
                // Sets an optional retry policy.
                .push(retry::layer(metrics.http_route_retry))
                // Sets an optional request timeout.
                .push(http::MakeTimeoutLayer::default())
                // Records per-route metrics.
                .push(metrics.http_route.to_layer::<classify::Response, _>())
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_map_target(Logical::mk_route)
                .into_inner(),
        ))
        // Strips headers that may be set by this proxy and add an outbound
        // canonical-dst-header. The response body is boxed unify the profile
        // stack's response type. withthat of to endpoint stack.
        .push(http::NewHeaderFromTarget::layer(CANONICAL_DST_HEADER))
        .push_on_response(
            svc::layers()
                .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                .push(http::BoxResponse::layer()),
        )
        .instrument(|l: &Logical| debug_span!("logical", dst = %l.addr()))
        .push_switch(
            Logical::should_resolve,
            svc::stack(endpoint)
                .push_on_response(http::BoxRequest::layer())
                .push_map_target(Endpoint::from_logical(
                    tls::NoClientTls::NotProvidedByServiceDiscovery,
                ))
                .into_inner(),
        )
        .into_inner()
}
