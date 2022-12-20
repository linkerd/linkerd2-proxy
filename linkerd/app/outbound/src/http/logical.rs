use super::{retry, CanonicalDstHeader, Concrete, Logical, ProfileRoute};
use crate::Outbound;
use linkerd_app_core::{
    classify, profiles,
    proxy::{api_resolve::ConcreteAddr, http},
    svc, Error,
};
use tracing::debug_span;

impl<N> Outbound<N> {
    pub fn push_http_logical<NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
            > + Send
            + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            let crate::Config {
                http_concrete_buffer,
                ..
            } = config;

            // If there's no route, use the logical service directly; otherwise
            // use the per-route stack.
            concrete
                .check_new_service::<Concrete, http::Request<http::BoxBody>>()
                .push_buffer_on_service::<http::Request<http::BoxBody>>(
                    "HTTP Concrete",
                    http_concrete_buffer.capacity,
                    http_concrete_buffer.failfast_timeout,
                )
                .push_on_service(http::BoxResponse::layer())
                .check_new_service::<Concrete, http::Request<http::BoxBody>>()
                .check_new_clone::<Concrete>()
                .push_map_target(Concrete::from)
                .push(profiles::NewConcreteCache::layer())
                .check_new_clone::<Logical>()
                .push(profiles::http::NewServiceRouter::<Concrete, _, _>::layer(
                    svc::layers()
                        // FIXME splitting/distribution should be done here:
                        // .push(profiles::split::layer())
                        .push_map_target(Concrete::from)
                        .push_map_target(|r: ProfileRoute| r.logical)
                        .push(http::insert::NewInsert::<ProfileRoute, _>::layer())
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
                        .push_on_service(http::BoxResponse::layer())
                        .push_map_target(|(route, logical)| ProfileRoute { logical, route }),
                ))
                .check_new_clone::<Logical>()
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header. The response body is boxed
                // unify the profile stack's response type with that of to
                // endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .instrument(|l: &Logical| debug_span!("logical", dst = %l.logical_addr))
                .check_new_clone::<Logical>()
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // .push_on_service(svc::BoxCloneService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

impl From<Logical> for Concrete {
    fn from(l: Logical) -> Self {
        let profiles::LogicalAddr(addr) = l.logical_addr.clone();
        Self::from((ConcreteAddr(addr), l))
    }
}
