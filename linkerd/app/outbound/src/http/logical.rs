use super::{retry, CanonicalDstHeader, Concrete, Endpoint, Logical, Route};
use crate::{endpoint, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    classify, config, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
        resolve::map_endpoint,
    },
    svc, Error, Infallible,
};
use tracing::debug_span;

impl<E> Outbound<E> {
    pub fn push_http_logical<B, ESvc, R>(self, resolve: R) -> Outbound<svc::ArcNewHttp<Logical, B>>
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
        self.map_stack(|config, rt, endpoint| {
            let config::ProxyConfig {
                buffer_capacity,
                cache_max_idle_age,
                dispatch_timeout,
                ..
            } = config.proxy;
            let watchdog = cache_max_idle_age * 2;

            let endpoint =
                endpoint.instrument(|e: &Endpoint| debug_span!("endpoint", server.addr = %e.addr));

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

            let concrete = endpoint
                .clone()
                .check_new_service::<Endpoint, http::Request<http::BoxBody>>()
                .push_on_service(
                    svc::layers().push(http::BoxRequest::layer()).push(
                        rt.metrics
                            .proxy
                            .stack
                            .layer(stack_labels("http", "balance.endpoint")),
                    ),
                )
                .check_new_service::<Endpoint, http::Request<_>>()
                // Resolve the service to its endpoints and balance requests over them.
                //
                // If the balancer has been empty/unavailable, eagerly fail requests.
                // When the balancer is in failfast, spawn the service in a background
                // task so it becomes ready without new requests.
                //
                // We *don't* ensure that the endpoint is driven to readiness here, because this
                // might cause us to continually attempt to reestablish connections without
                // consulting discovery to see whether the endpoint has been removed. Instead, the
                // endpoint layer spawns each _connection_ attempt on a background task, but the
                // decision to attempt the connection must be driven by the balancer.
                .push(resolve::layer(resolve, watchdog))
                .push_on_service(
                    svc::layers()
                        .push(http::balance::layer(
                            crate::EWMA_DEFAULT_RTT,
                            crate::EWMA_DECAY,
                        ))
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "balancer")),
                        )
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(svc::FailFast::layer("HTTP Balancer", dispatch_timeout))
                        .push(http::BoxResponse::layer()),
                )
                .check_make_service::<Concrete, http::Request<_>>()
                .push(svc::MapErr::layer(Into::into))
                // Drives the initial resolution via the service's readiness.
                .into_new_service()
                // The concrete address is only set when the profile could be
                // resolved. Endpoint resolution is skipped when there is no
                // concrete address.
                .instrument(|c: &Concrete| debug_span!("concrete", addr = %c.resolve))
                .push_map_target(Concrete::from)
                .push(svc::ArcNewService::layer());

            // Distribute requests over a distribution of balancers via a
            // traffic split.
            //
            // If the traffic split is empty/unavailable, eagerly fail requests.
            // When the split is in failfast, spawn the service in a background
            // task so it becomes ready without new requests.
            let split = concrete
                .check_new_service::<(ConcreteAddr, Logical), _>()
                .push(profiles::split::layer())
                .push_on_service(
                    svc::layers()
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "logical")),
                        )
                        .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity),
                )
                .push_cache(cache_max_idle_age)
                .push_on_service(http::BoxResponse::layer());

            let routes = split
                .clone()
                .push_map_target(|r: Route| r.logical)
                .push_on_service(http::BoxRequest::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_route_actual
                        .to_layer::<classify::Response, _, Route>(),
                )
                // Depending on whether or not the request can be retried, it
                // may have one of two `Body` types. This layer unifies any
                // `Body` type into `BoxBody` so that the rest of the stack
                // doesn't have to implement `Service` for requests with both
                // body types.
                .push_on_service(http::BoxRequest::erased())
                .push_http_insert_target::<profiles::http::Route>()
                // Sets an optional retry policy.
                .push(retry::layer(rt.metrics.proxy.http_route_retry.clone()))
                // Sets an optional request timeout.
                .push(http::NewTimeout::layer())
                // Records per-route metrics.
                .push(
                    rt.metrics
                        .proxy
                        .http_route
                        .to_layer::<classify::Response, _, _>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_cache(config.proxy.cache_max_idle_age)
                .push_on_service(
                    svc::layers()
                        .push(http::Retain::layer())
                        .push(http::BoxResponse::layer()),
                )
                .into_inner();

            split
                .push_switch(
                    |(route, logical): (Option<profiles::http::Route>, Logical)| -> Result<_, Infallible> {
                        match route {
                            None => Ok(svc::Either::A(logical)),
                            Some(route) => Ok(svc::Either::B(Route { route, logical })),
                        }
                    },
                    routes,
                )
                .push(profiles::http::route_request::layer())
                .push_on_service(http::BoxRequest::layer())
                // Strips headers that may be set by this proxy and add an outbound
                // canonical-dst-header. The response body is boxed unify the profile
                // stack's response type with that of to endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .push_on_service(http::BoxResponse::layer())
                .instrument(|l: &Logical| debug_span!("logical", dst = %l.logical_addr))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
