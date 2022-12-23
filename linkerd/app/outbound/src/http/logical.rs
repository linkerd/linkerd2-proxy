use super::{retry, CanonicalDstHeader, Concrete, Logical, ProfileRoute};
use crate::{stack_labels, Outbound};
use linkerd_app_core::{
    classify, profiles,
    proxy::{api_resolve::ConcreteAddr, http},
    svc, Error, Infallible,
};
use tracing::debug_span;

impl<N> Outbound<N> {
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
                        .push_buffer("HTTP Logical", http_logical_buffer),
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
