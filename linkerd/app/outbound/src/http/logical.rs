use super::{retry, CanonicalDstHeader, Concrete, Endpoint, Logical, ProfileRoute};
use crate::{endpoint, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    classify, profiles,
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
            >,
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
                orig_dst_idle_timeout,
                ..
            } = config;
            let watchdog = *orig_dst_idle_timeout * 2;

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
                // .instrument(|e: &Endpoint| debug_span!("endpoint", server.addr = %e.addr))
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
                .push(svc::ArcNewService::layer())
        })
    }

    pub fn push_http_logical<NSvc>(self) -> Outbound<svc::ArcNewHttp<Logical>>
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            let crate::Config {
                orig_dst_idle_timeout,
                http_logical_buffer,
                ..
            } = config;

            // Distribute requests over a distribution of balancers via a
            // traffic split.
            //
            // If the traffic split is empty/unavailable, eagerly fail requests.
            // When the split is in failfast, spawn the service in a background
            // task so it becomes ready without new requests.
            let logical = concrete
                .push_map_target(Concrete::from)
                .check_new_service::<(ConcreteAddr, Logical), _>()
                .push(profiles::split::layer())
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "logical")),
                        )
                        .push_buffer("HTTP Logical", http_logical_buffer.capacity, http_logical_buffer.failfast_timeout),
                )
                .push_cache(*orig_dst_idle_timeout);

            // If there's no route, use the logical service directly; otherwise
            // use the per-route stack.
            logical
                .clone()
                .push_switch(
                    |(route, logical): (Option<profiles::http::Route>, Logical)| -> Result<_, Infallible> {
                        match route {
                            None => Ok(svc::Either::A(logical)),
                            Some(route) => Ok(svc::Either::B(ProfileRoute { route, logical })),
                        }
                    },
                    logical
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
                .push(profiles::http::NewServiceRouter::layer())
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
