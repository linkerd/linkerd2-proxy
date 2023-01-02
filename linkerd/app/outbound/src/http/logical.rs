use super::{retry, CanonicalDstHeader, Concrete, Logical};
use crate::Outbound;
use linkerd_app_core::{
    classify, metrics, profiles,
    profiles::{self, Profile},
    proxy::http,
    svc, Error, NameAddr,
};
use linkerd_distribute::{Backends, CacheNewDistribute, Distribute, Distribution};
use linkerd_router as router;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::debug_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProfileRoute {
    logical: Logical,
    route: profiles::http::Route,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    logical: NameAddr,
    rx: watch::Receiver<Config>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Config {
    logical: NameAddr,
    backends: Backends<BackendConfig>,
    route_keys: router::RouteKeys<RouteKey>,
    routes: Arc<ahash::AHashMap<profiles::http::RequestMatch, RouteKey>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendConfig {
    logical: NameAddr,
    concrete: NameAddr,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteKey {
    logical: NameAddr,
    config: profiles::http::Route,
    distribution: Distribution<BackendConfig>,
}

#[derive(Clone, Debug)]
struct NewRoute<L, N, S>(
    router::NewRouteWatch<Profile, RouteKey, L, CacheNewDistribute<BackendConfig, N, S>>,
);

impl<T, L, N, S> svc::NewService<T> for NewRoute<L, N, S>
where
    T: svc::Param<profiles::LogicalAddr>,
    T: svc::Param<watch::Receiver<Profile>>,
    N: svc::NewService<BackendConfig>,
    L: svc::layer::Layer<Distribute<BackendConfig, N::Service>>,
    L::Service: svc::NewService<RouteKey, Service = S>,
{
    type Service = router::Route<RouteKey, Profile, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let profiles::LogicalAddr(logical) = target.param();

        let mut profile_rx: watch::Receiver<Profile> = target.param();
        let (mut tx, rx) = watch::channel(Target::new(logical, &*profile_rx.borrow_and_update()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => return,
                    res = profile_rx.changed() => {
                        if res.is_err() {
                            return;
                        }
                    }
                }

                if tx
                    .send(Target::new(logical, &*profile_rx.borrow_and_update()))
                    .is_err()
                {
                    return;
                }
            }
        });

        self.0.new_service(Target { logical, rx })
    }
}

impl svc::Param<profiles::LogicalAddr> for Target {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.logical.clone())
    }
}

impl svc::Param<profiles::LogicalAddr> for Config {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.logical.clone())
    }
}

impl svc::Param<Distribution<BackendConfig>> for RouteKey {
    fn param(&self) -> Distribution<BackendConfig> {
        self.distribution.clone()
    }
}
