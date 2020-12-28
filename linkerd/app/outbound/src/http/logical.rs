use super::{Concrete, Endpoint, Logical};
use crate::{resolve, stack_labels};
use linkerd2_app_core::{
    classify,
    config::ProxyConfig,
    metrics, profiles,
    proxy::{api_resolve::Metadata, core::Resolve, http},
    retry, svc,
    transport::tls::ReasonForNoPeerName,
    Addr, Error, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
};
use tracing::debug_span;

pub fn stack<B, E, S, R>(
    config: &ProxyConfig,
    endpoint: E,
    resolve: R,
    metrics: metrics::Proxy,
) -> impl svc::NewService<
    Logical,
    Service = impl tower::Service<
        http::Request<B>,
        Response = http::Response<http::boxed::BoxBody>,
        Error = Error,
        Future = impl Send,
    > + Send,
> + Unpin
       + Clone
       + Send
where
    B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
    B::Data: Send + 'static,
    E: svc::NewService<Endpoint, Service = S> + Clone + Send + Sync + Unpin + 'static,
    S: tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    S::Future: Send,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + 'static,
    R::Future: Unpin + Send,
    R::Resolution: Unpin + Send,
{
    let ProxyConfig {
        buffer_capacity,
        cache_max_idle_age,
        dispatch_timeout,
        ..
    } = config.clone();
    let watchdog = cache_max_idle_age * 2;

    svc::stack(endpoint.clone())
        .check_new_service::<Endpoint, http::Request<http::boxed::BoxBody>>()
        .push_on_response(
            svc::layers()
                .push(svc::layer::mk(svc::SpawnReady::new))
                .push(
                    metrics
                        .stack
                        .layer(stack_labels("http", "balance.endpoint")),
                )
                .push(http::boxed::BoxRequest::layer()),
        )
        .check_new_service::<Endpoint, http::Request<_>>()
        .push(resolve::layer(resolve, watchdog))
        .check_service::<Concrete>()
        .push_on_response(
            svc::layers()
                .push(http::balance::layer(
                    crate::EWMA_DEFAULT_RTT,
                    crate::EWMA_DECAY,
                ))
                .push(svc::layer::mk(svc::SpawnReady::new))
                // If the balancer has been empty/unavailable for 10s, eagerly fail
                // requests.
                .push(svc::FailFast::layer("HTTP Balancer", dispatch_timeout))
                .push(metrics.stack.layer(stack_labels("http", "concrete"))),
        )
        .into_new_service()
        .check_new_service::<Concrete, http::Request<_>>()
        .instrument(|c: &Concrete| match c.resolve.as_ref() {
            None => debug_span!("concrete"),
            Some(addr) => debug_span!("concrete", %addr),
        })
        .check_new_service::<Concrete, http::Request<_>>()
        // The concrete address is only set when the profile could be
        // resolved. Endpoint resolution is skipped when there is no
        // concrete address.
        .push_map_target(Concrete::from)
        .check_new_service::<(Option<Addr>, Logical), http::Request<_>>()
        .push(profiles::split::layer())
        .check_new_service::<Logical, http::Request<_>>()
        // Drives concrete stacks to readiness and makes the split
        // cloneable, as required by the retry middleware.
        .push_on_response(
            svc::layers()
                .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                .push_spawn_buffer(buffer_capacity),
        )
        .check_new_service::<Logical, http::Request<_>>()
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
        .check_new_service::<Logical, http::Request<_>>()
        .push(http::NewHeaderFromTarget::layer(CANONICAL_DST_HEADER))
        .push_on_response(
            svc::layers()
                // Strips headers that may be set by this proxy.
                .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                .push(http::boxed::BoxResponse::layer()),
        )
        .instrument(|l: &Logical| debug_span!("logical", dst = %l.addr()))
        .check_new_service::<Logical, http::Request<_>>()
        .push_switch(
            Logical::should_resolve,
            svc::stack(endpoint)
                .push_on_response(http::boxed::BoxRequest::layer())
                .push_map_target(Endpoint::from_logical(
                    ReasonForNoPeerName::NotProvidedByServiceDiscovery,
                ))
                .into_inner(),
        )
        .into_inner()
}
