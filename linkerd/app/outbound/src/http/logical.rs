use super::{CanonicalDstHeader, Concrete, Endpoint, Logical};
use crate::{endpoint, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    classify, config, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
        resolve::map_endpoint,
    },
    retry, svc, Error, Never,
};
use tracing::debug_span;

impl<E> Outbound<E> {
    pub fn push_http_logical<B, ESvc, R>(self, resolve: R) -> Outbound<svc::BoxNewHttp<Logical, B>>
    where
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Unpin + Send + 'static,
        B::Data: Send + 'static,
        E: svc::NewService<Endpoint, Service = ESvc> + Clone + Send + Sync + 'static,
        ESvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        ESvc::Error: Into<Error>,
        ESvc::Future: Send,
        R: Resolve<ConcreteAddr, Error = Error, Endpoint = Metadata>,
        R: Clone + Send + Sync + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        let Self {
            config,
            runtime: rt,
            stack: endpoint,
        } = self;
        let config::ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = config.proxy;
        let watchdog = cache_max_idle_age * 2;

        let endpoint =
            endpoint.instrument(|e: &Endpoint| debug_span!("endpoint", server.addr = %e.addr));

        let identity_disabled = rt.identity.is_none();
        let resolve = svc::stack(resolve.into_service())
            .check_service::<ConcreteAddr>()
            .push_request_filter(|c: Concrete| Ok::<_, Never>(c.resolve))
            .push(svc::layer::mk(move |inner| {
                map_endpoint::Resolve::new(endpoint::FromMetadata { identity_disabled }, inner)
            }))
            .check_service::<Concrete>()
            .into_inner();

        // Build a retry policy with retry metrics and the maximum buffered
        // request body length from the config.
        let retry_policy = retry::NewRetry::new(rt.metrics.http_route_retry.clone())
            .clone_requests_via(retry::BufferBody::with_max_length(
                config.max_retry_length_bytes,
            ));

        let stack = endpoint
            .clone()
            .check_new_service::<Endpoint, http::Request<http::BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    .push(
                        rt.metrics
                            .stack
                            .layer(stack_labels("http", "balance.endpoint")),
                    )
                    // Ensure individual endpoints are driven to readiness so that
                    // the balancer need not drive them all directly.
                    .push(svc::layer::mk(svc::SpawnReady::new)),
            )
            .check_new_service::<Endpoint, http::Request<_>>()
            // Resolve the service to its endpoints and balance requests over them.
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
                    .push(rt.metrics.stack.layer(stack_labels("http", "balancer")))
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(svc::FailFast::layer("HTTP Balancer", dispatch_timeout))
                    .push(http::BoxResponse::layer()),
            )
            .check_make_service::<Concrete, http::Request<_>>()
            .push(svc::MapErrLayer::new(Into::into))
            // Drives the initial resolution via the service's readiness.
            .into_new_service()
            // The concrete address is only set when the profile could be
            // resolved. Endpoint resolution is skipped when there is no
            // concrete address.
            .instrument(|c: &Concrete| debug_span!("concrete", addr = %c.resolve))
            .push_map_target(Concrete::from)
            .push(svc::BoxNewService::layer())
            // Distribute requests over a distribution of balancers via a
            // traffic split.
            //
            // If the traffic split is empty/unavailable, eagerly fail requests.
            // When the split is in failfast, spawn the service in a background
            // task so it becomes ready without new requests.
            .push(profiles::split::layer())
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(rt.metrics.stack.layer(stack_labels("http", "logical")))
                    .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                    .push_spawn_buffer(buffer_capacity),
            )
            .push_cache(cache_max_idle_age)
            // Note: routes can't exert backpressure.
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    .push(
                        rt.metrics
                            .http_route_actual
                            .to_layer::<classify::Response, _>(),
                    )
                    // Sets an optional retry policy.
                    .push(retry::layer(retry_policy))
                    .push_on_response(retry::replay::layer())
                    // Sets an optional request timeout.
                    .push(http::MakeTimeoutLayer::default())
                    // Records per-route metrics.
                    .push(rt.metrics.http_route.to_layer::<classify::Response, _>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .push_map_target(Logical::mk_route)
                    .into_inner(),
            ))
            // Strips headers that may be set by this proxy and add an outbound
            // canonical-dst-header. The response body is boxed unify the profile
            // stack's response type. withthat of to endpoint stack.
            .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
            .push_on_response(svc::layers().push(http::BoxResponse::layer()))
            .instrument(|l: &Logical| debug_span!("logical", dst = %l.logical_addr))
            .push_on_response(svc::BoxService::layer())
            .push(svc::BoxNewService::layer());

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
