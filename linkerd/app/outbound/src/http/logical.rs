use super::{retry, CanonicalDstHeader, Concrete, Endpoint, Logical, PolicyRoute, ProfileRoute};
use crate::{
    endpoint,
    policy::{self, Policy},
    resolve, stack_labels, Outbound,
};
use linkerd_app_core::{
    classify, config, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        http,
        resolve::map_endpoint,
    },
    svc,
    transport::OrigDstAddr,
    Error, Infallible,
};
use tracing::debug_span;

impl<E> Outbound<E> {
    pub fn push_http_logical<ESvc, R, P>(
        self,
        resolve: R,
        policies: P,
    ) -> Outbound<svc::ArcNewHttp<Logical>>
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
        P: svc::Service<OrigDstAddr, Response = policy::Receiver>,
        P: Clone + Send + Sync + 'static,
        P::Future: Send,
        Error: From<P::Error>,
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
            let logical = concrete
                .clone()
                .check_new_service::<(ConcreteAddr, Logical), _>()
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
            let policy_route = concrete
                .check_new_service::<(ConcreteAddr, Logical), _>()
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
                .check_new_clone::<PolicyRoute>();

                let policy = logical
                    .push_switch(
                        |(route, logical): (Option<policy::RoutePolicy>, Logical)| -> Result<_, Infallible> {
                            match route {
                                None => Ok(svc::Either::A(logical)),
                                Some(route) => Ok(svc::Either::B(PolicyRoute { route, logical })),
                            }
                        },
                        policy_route,
                    )
                    .check_new_service::<(Option<policy::RoutePolicy>, Logical), http::Request<_>>()
                    .push(policy::http::NewServiceRouter::layer())
                    .push_map_target(|(policy, logical): (Policy, Logical)| {
                        // for now, just log the client policy rather than actually
                        // doing anything...
                        tracing::info!(?logical, ?policy);
                        // Add the discovered policy to the logical target
                        Logical {
                            policy: Some(policy),
                            ..logical
                        }
                    })
                    .instrument(|(policy, _): &(Policy, Logical)| debug_span!("policy", addr = %policy.dst))
                    .check_new_service::<(Policy, Logical), http::Request<_>>();

                let logical = policy.push_switch(
                    |(policy, logical): (Policy, Logical)| -> Result<_, Infallible> {
                        let is_empty = {
                            let policy = policy.policy.borrow();
                            policy.http_routes.is_empty() && policy.backends.is_empty()
                        };
                        if is_empty {
                            // No client policy is defined for this destination,
                            // so continue with the service profile stack.
                            return Ok(svc::Either::B(logical));
                        }

                        Ok(svc::Either::A((policy, logical)))
                    },
                    profile_route,
                )
                .check_new_service::<(Policy, Logical), http::Request<_>>();

                logical
                    // discover client policies for the original destination address
                    .push(policy::Discover::layer(policies, cache_max_idle_age))
                    .check_make_service::<Logical, http::Request<_>>()
                    // policies must be resolved before the logical stack can be
                    // built, so the policy layer is a `MakeService`. drive the
                    // initial resolution via the service's readiness here.
                    .into_new_service()
                    .check_new_service::<Logical, _>()
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
}
