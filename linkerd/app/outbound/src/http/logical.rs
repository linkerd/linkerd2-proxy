use super::{retry, CanonicalDstHeader, Concrete, Endpoint, Logical, PolicyRoute, ProfileRoute};
use crate::{endpoint, policy, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    cache, classify, config, profiles,
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
        let endpoint = self.map_stack(|_, _, endpoint| {
            endpoint.instrument(|e: &Endpoint| debug_span!("endpoint", server.addr = %e.addr))
        });

        endpoint.clone()
            .push_http_concrete(resolve)
            .map_stack(|config, rt, concrete| {
                let config::ProxyConfig {
                    buffer_capacity,
                    cache_max_idle_age,
                    dispatch_timeout,
                    ..
                } = config.proxy;
                // Distribute requests over a distribution of balancers via a
                // traffic split.
                //
                // If the traffic split is empty/unavailable, eagerly fail requests.
                // When the split is in failfast, spawn the service in a background
                // task so it becomes ready without new requests.
                let logical = concrete
                    .clone()
                    .push_map_target(Concrete::from)
                    .push(policy::split::NewDynamicSplit::layer())
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
                    .push_cache(cache_max_idle_age);

                let profile_route = logical
                    .clone()
                    // If there's no route, use the logical service directly; otherwise
                    // use the per-route stack.
                    .push_switch(
                        |(route, logical): (Option<profiles::http::Route>, Logical)| -> Result<_, Infallible> {
                            match route {
                                None => Ok(svc::Either::A(logical)),
                                Some(route) => Ok(svc::Either::B(ProfileRoute { route, logical })),
                            }
                        },
                        logical.clone()
                            .push_map_target(|r: ProfileRoute| r.logical)
                            .push_on_service(http::BoxRequest::layer())
                            .push(
                                rt.metrics
                                    .proxy
                                    .http_profile_route_actual
                                    .to_layer::<classify::Response, _, ProfileRoute>(),
                            )
                            // Depending on whether or not the request can be
                            // retried, it may have one of two `Body` types. This
                            // layer unifies any `Body` type into `BoxBody`.
                            .push_on_service(http::BoxRequest::erased())
                            .push_http_insert_target::<profiles::http::Route>()
                            // Sets an optional retry policy.
                            .push(retry::layer(rt.metrics.proxy.http_profile_route_retry.clone()))
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
                                    .push(svc::BoxCloneService::layer())
                            )
                            .into_inner(),
                    )
                    .push(profiles::http::NewServiceRouter::layer());

                // for now, the client-policy route stack is just a fixed traffic split
                let policy = concrete
                    .push_map_target(Concrete::from)
                    // Any per-route policy has to go between the split layer and
                    // the concrete stack cache, since we _don't_ want to share
                    // client policy across routes that have the same backend!
                    .push_map_target(|(concrete, PolicyRoute { route: _, logical }): (ConcreteAddr, PolicyRoute)| {
                        (concrete, logical)
                    })
                    .push(policy::split::layer())
                    .push_on_service(
                        svc::layers()
                            .push(svc::layer::mk(svc::SpawnReady::new))
                            .push(
                                rt.metrics
                                    .proxy
                                    .stack
                                    .layer(stack_labels("http", "HTTPRoute")),
                            )
                            .push(svc::FailFast::layer("HTTPRoute", dispatch_timeout))
                            .push_spawn_buffer(buffer_capacity),
                    )
                    .check_new_clone::<PolicyRoute>()
                    .push_map_target(|(route, logical)| {
                        tracing::debug!(?route);
                        PolicyRoute { route, logical }
                    })
                    .push(policy::http::NewServiceRouter::layer())
                    .push_map_target(|logical: Logical| {
                        {
                            let policy = logical
                                .policy
                                .as_ref()
                                .expect("policy stack should not be built if there's no policy")
                                .borrow();

                            // for now, log the client policy at INFO for
                            // development purposes
                            tracing::info!(
                                dst = ?logical.logical_addr,
                                ?policy.backends,
                                ?policy.http_routes,
                            );
                        }
                        logical

                    })
                    .instrument(|_: &Logical| debug_span!("policy"))
                    .check_new_service::<Logical, http::Request<_>>();

                    let logical = policy.push_switch(
                        |logical: Logical| -> Result<_, Infallible> {
                            if logical.policy.is_some() {
                                Ok(svc::Either::A(logical))
                            } else {
                                Ok(svc::Either::B(logical))
                            }
                        },
                        profile_route,
                    )
                    .check_new_service::<Logical, http::Request<_>>();

                    logical
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
