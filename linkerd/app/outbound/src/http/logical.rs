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
                discovery_idle_timeout,
                http_request_buffer,
                ..
            } = config;

            let split = concrete
                .push_map_target(Concrete::from)
                // This failfast is needed to ensure that a single unavailable
                // balancer doesn't block the entire split.
                //
                // TODO(ver) remove this when we replace `split` with
                // `distribute`.
                .push_on_service(svc::FailFast::layer(http_request_buffer.failfast_timeout))
                .check_new_service::<(ConcreteAddr, Logical), _>()
                // Distribute requests over a distribution of balancers via a
                // traffic split.
                .push(profiles::split::layer())
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(stack_labels("http", "logical")),
                        )
                        .push_buffer("HTTP Logical", http_request_buffer),
                )
                // TODO(ver) this should not be a generalized time-based evicting cache.
                .push_cache(*discovery_idle_timeout);

            // If there's no route, use the logical service directly; otherwise
            // use the per-route stack.
            let route = split.clone()
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .push_map_target(|r: ProfileRoute| r.logical)
                .push_on_service(http::BoxRequest::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route_actual
                        .to_layer::<classify::Response, _, ProfileRoute>(),
                )
                .check_new_service::<ProfileRoute, http::Request<_>>()
                // Depending on whether or not the request can be
                // retried, it may have one of two `Body` types. This
                // layer unifies any `Body` type into `BoxBody`.
                .push_on_service(http::BoxRequest::erased())
                .push_http_insert_target::<profiles::http::Route>()
                // Sets an optional retry policy.
                .push(retry::layer(rt.metrics.proxy.http_profile_route_retry.clone()))
                .check_new_service::<ProfileRoute, http::Request<http::BoxBody>>()
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
                .check_new_service::<ProfileRoute, http::Request<http::BoxBody>>();

            split
                .push_switch(
                    |(route, logical): (Option<profiles::http::Route>, Logical)| -> Result<_, Infallible> {
                        Ok(match route {
                            None => svc::Either::A(logical),
                            Some(route) => svc::Either::B(ProfileRoute { route, logical }),
                        })
                    },
                    route.into_inner(),
                )
                .push(profiles::http::NewServiceRouter::layer())
                // Strips headers that may be set by this proxy and add an
                // outbound canonical-dst-header. The response body is boxed
                // unify the profile stack's response type with that of to
                // endpoint stack.
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .instrument(|l: &Logical| debug_span!("logical", service = %l.logical_addr))
                .push_on_service(svc::BoxService::layer())
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
