mod router;

use self::router::NewRouter;
use super::{retry, CanonicalDstHeader, Concrete, Logical};
use crate::{stack_labels, Outbound};
use linkerd_app_core::{
    classify, metrics, profiles,
    proxy::{api_resolve::ConcreteAddr, http},
    svc, Error, Infallible,
};
use tracing::debug_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProfileRoute {
    logical: Logical,
    route: profiles::http::Route,
}

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
            > + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, concrete| {
            // If there's no route, use the logical service directly; otherwise
            // use the per-route stack.
            concrete
                .push(NewRouter::<_, _, Concrete>::layer(
                    svc::layers()
                        // Maintain a per-route distributor over concrete
                        // backends from the (above) concrete cache.
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
                        .push_map_target(|(route, logical)| ProfileRoute { logical, route }),
                ))
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header. The response body is boxed
                // unify the profile stack's response type with that of to
                // endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
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
