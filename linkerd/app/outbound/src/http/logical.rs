use super::{retry, CanonicalDstHeader, Concrete, Logical};
use crate::logical::router;
use crate::Outbound;
use linkerd_app_core::{classify, metrics, profiles, proxy::http, svc, Error};
use tracing::debug_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProfileRoute {
    logical: Logical,
    route: profiles::http::Route,
}

type NewRoute<N, R> =
    router::NewRoute<N, R, Concrete, profiles::http::RouteSet, profiles::http::Route>;

impl<N> Outbound<N> {
    // TODO(ver) make the outer target type generic/parameterized.
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
            > + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            let route = svc::layers()
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
                        .to_layer::<classify::Response, _, ProfileRoute>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                .push_on_service(http::BoxResponse::layer())
                .push_map_target(|(route, logical)| ProfileRoute { logical, route })
                // Only build a route service when it is used.
                .push(svc::NewLazy::layer());

            concrete
                .check_new_service::<Concrete, _>()
                .push(NewRoute::layer(route))
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header. The response body is boxed
                // unify the profile stack's response type with that of to
                // endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                // This caches each logical stack so that it can be reused
                // across per-connection HTTP server stacks (i.e. created by the
                // `DetectService`).
                //
                // TODO(ver) Update the detection stack so this dynamic caching
                // can be removed.
                //
                // XXX(ver) This cache key includes the HTTP version. Should it?
                .push_cache(config.discovery_idle_timeout)
                .instrument(|l: &Logical| debug_span!("logical", service = %l.logical_addr))
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl ProfileRoute ===

impl svc::Param<profiles::http::Route> for ProfileRoute {
    fn param(&self) -> profiles::http::Route {
        self.route.clone()
    }
}

impl svc::Param<metrics::ProfileRouteLabels> for ProfileRoute {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(self.logical.logical_addr.clone(), &self.route)
    }
}

impl svc::Param<http::ResponseTimeout> for ProfileRoute {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.route.timeout())
    }
}

impl classify::CanClassify for ProfileRoute {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl svc::Param<router::Distribution> for ProfileRoute {
    fn param(&self) -> router::Distribution {
        debug_assert!(!self.route.backends().as_ref().is_empty());
        router::Distribution::random_available(
            self.route
                .backends()
                .iter()
                .map(|profiles::Backend { addr, weight }| (addr.clone(), *weight)),
        )
        .expect("distribution must be valid")
    }
}
