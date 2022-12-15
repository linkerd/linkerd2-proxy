use super::{retry, CanonicalDstHeader, Concrete, Endpoint, Logical, PolicyRoute};
use crate::{endpoint, policy, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    cache, classify, config,
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
    pub fn push_http_logical<ESvc, R>(self, resolve: R) -> Outbound<svc::ArcNewHttp<Logical>>
    where
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
        self.push_http_concrete(resolve)
            .push_http_route()
            .map_stack(|_, _, route| {
                route
                    .check_new_service::<Logical, http::Request<_>>()
                    // Strips headers that may be set by this proxy and add an
                    // outbound canonical-dst-header. The response body is boxed
                    // unify the profile stack's response type with that of to
                    // endpoint stack.
                    .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                    .instrument(|l: &Logical| debug_span!("logical", dst = %l.logical_addr))
                    .push_on_service(svc::BoxService::layer())
                    .push(svc::ArcNewService::layer())
            })
    }
    fn push_http_route<ESvc>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                Logical,
                Service = impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Future = impl Send,
                    Error = Error,
                > + Send
                              + 'static,
            > + Clone,
    >
    where
        E: svc::NewService<Concrete, Service = ESvc> + Clone + Send + Sync + 'static,
        ESvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + Clone
            + 'static,
        ESvc::Error: Into<Error>,
        ESvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            let config::ProxyConfig {
                buffer_capacity,
                cache_max_idle_age,
                dispatch_timeout,
                ..
            } = config.proxy;

            // If the route does not have a list of per-route backends, use a
            // dynamically updating traffic split based on the logical target.
            let profile_split = concrete
                .clone()
                .push_map_target(Concrete::from)
                .push(policy::split::NewDynamicSplit::layer());

            // Distribute requests over a distribution of balancers via a
            // traffic split.
            //
            // If the traffic split is empty/unavailable, eagerly fail requests.
            // When the split is in failfast, spawn the service in a background
            // task so it becomes ready without new requests.
            let split = concrete
                .push_map_target(
                    |(concrete, PolicyRoute { route: _, logical }): (ConcreteAddr, PolicyRoute)| {
                        Concrete::from((concrete, logical))
                    },
                )
                .push(policy::split::layer())
                .push_switch(
                    |policy: PolicyRoute| -> Result<_, Infallible> {
                        // If the HTTP route defines a list of backends, build a
                        // traffic split for that backend list (an HTTPRoute
                        // traffic split). Otherwise, build  a traffic split for
                        // the top-level list of backends on the `Logical`
                        // target (a ServiceProfile traffic split).
                        if policy.route.backends.is_empty() {
                            Ok(svc::Either::B(policy.logical))
                        } else {
                            Ok(svc::Either::A(policy))
                        }
                    },
                    profile_split,
                )
                .push_on_service(
                    svc::layers()
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "concrete")),
                        )
                        .push(svc::FailFast::layer("HTTP Concrete", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity),
                )
                .push_cache(cache_max_idle_age);

            split
                .push_on_service(http::BoxRequest::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route_actual
                        .to_layer::<classify::Response, _, PolicyRoute>(),
                )
                // Depending on whether or not the request can be
                // retried, it may have one of two `Body` types. This
                // layer unifies any `Body` type into `BoxBody`.
                .push_on_service(http::BoxRequest::erased())
                .push_http_insert_target::<policy::http::RoutePolicy>()
                // Sets an optional retry policy.
                .push(retry::layer(
                    rt.metrics.proxy.http_profile_route_retry.clone(),
                ))
                // Sets an optional request timeout.
                .push(http::NewTimeout::layer())
                // Records per-route metrics.
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route
                        .to_layer::<classify::Response, _, _>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_on_service(
                    svc::layers()
                        .push(http::BoxResponse::layer())
                        .push(svc::BoxCloneService::layer()),
                )
                .check_new_service::<PolicyRoute, _>()
                .push_map_target(|(route, logical)| PolicyRoute { route, logical })
                .push(policy::http::NewServiceRouter::layer())
                .check_new_service::<Logical, _>()
        })
    }

    /// Push the concrete address stack, including destination resolution and
    /// load balancing.
    ///
    /// This stack contains a cache keyed on concrete addresses, so that when
    /// backends in different HTTP routes refer to the same concrete address,
    /// the same load balancer is shared across those backends.
    fn push_http_concrete<ESvc, R>(
        self,
        resolve: R,
    ) -> Outbound<
        cache::NewCachedService<
            Concrete,
            impl svc::NewService<
                    Concrete,
                    Service = impl svc::Service<
                        http::Request<http::BoxBody>,
                        Response = http::Response<http::BoxBody>,
                        Future = impl Send,
                        Error = impl Into<Error>,
                    > + Clone
                                  + Send
                                  + 'static,
                > + Clone,
        >,
    >
    where
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
                // Cache and buffer the concrete address client stacks created
                // for each backend in the HTTPRoute, so that connections are
                // shared across multiple route rules that share a backend.
                // TODO(eliza): add a test that these connections are cached
                // across routes.
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "concrete")),
                        )
                        .push_spawn_buffer(buffer_capacity),
                )
                .push_cache(cache_max_idle_age)
                .check_new_service::<Concrete, http::Request<_>>()
        })
    }
}
