use linkerd_app_core::{
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc::{self, layer, NewSpawnWatch},
    NameAddr,
};
use linkerd_distribute::Backends;
use linkerd_router::{NewRoute, RouteKeys, SelectRoute};
use std::sync::Arc;

pub(crate) type Matches<M, K> = Arc<[(M, K)]>;
pub(crate) type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;
type CacheNewDistribute<N, S> = linkerd_distribute::CacheNewDistribute<NameAddr, N, S>;

pub type NewRouteDistribute<L, N, S> =
    NewSpawnWatch<HttpParams, Profile, NewRoute<HttpRouteParams, L, CacheNewDistribute<N, S>>>;

// === impl NewRoute ===

// TODO impl Param<RouteKeys<>> for Profile
// TODO impl Param<Backends<ConcreteAddr>> for Profile
// Or map here...
pub fn http<L: Clone, N, S>(
    route_layer: L,
) -> impl layer::Layer<N, Service = NewRouteDistribute<L, N, S>> + Clone {
    layer::mk(move |inner| {
        let route = NewRoute::<HttpRouteParams, L, _>::new(
            route_layer.clone(),
            CacheNewDistribute::new(inner),
        );
        NewSpawnWatch::<HttpParams, Profile, _>::new(route)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpParams {
    logical: crate::http::Logical,
    routes: Arc<[(profiles::http::RequestMatch, HttpRouteParams)]>,
    keys: RouteKeys<HttpRouteParams>,
    backends: Backends<crate::http::Concrete>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpRouteParams {
    logical: crate::http::Logical,
    profile: profiles::http::Route,
    distribution: Distribution,
}

impl From<(Profile, crate::http::Logical)> for HttpParams {
    fn from((profile, logical): (Profile, crate::http::Logical)) -> Self {
        let routes = profile
            .http_routes
            .iter()
            .cloned()
            .map(|(m, profile)| {
                (
                    m,
                    HttpRouteParams {
                        logical: logical.clone(),
                        distribution: Distribution::random_available(
                            profile.targets().iter().map(|t| (t.addr.clone(), t.weight)),
                        )
                        .expect("distribution must be valid"),
                        profile,
                    },
                )
            })
            .collect::<Arc<[(_, _)]>>();
        let backends = profile
            .target_addrs
            .iter()
            .map(|addr| super::Concrete {
                resolve: ConcreteAddr(addr.clone()),
                logical,
            })
            .collect();
        Self {
            backends,
            keys: routes.iter().map(|(_, r)| r.clone()).collect(),
            logical,
            routes,
        }
    }
}

impl svc::Param<RouteKeys<HttpRouteParams>> for HttpParams {
    fn param(&self) -> RouteKeys<HttpRouteParams> {
        self.keys.clone()
    }
}

impl svc::Param<Backends<crate::http::Concrete>> for HttpParams {
    fn param(&self) -> Backends<crate::http::Concrete> {
        self.backends.clone()
    }
}

impl svc::Param<profiles::LogicalAddr> for HttpParams {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}

impl svc::Param<profiles::LogicalAddr> for HttpRouteParams {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}

impl svc::Param<Distribution> for HttpRouteParams {
    fn param(&self) -> Distribution {
        self.distribution.clone()
    }
}

impl svc::Param<profiles::http::Route> for HttpRouteParams {
    fn param(&self) -> profiles::http::Route {
        self.profile.clone()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

impl<B> SelectRoute<http::Request<B>> for HttpParams {
    type Key = HttpRouteParams;
    type Error = NoRoute;

    fn select<'r>(&self, req: &'r http::Request<B>) -> Result<&Self::Key, Self::Error> {
        profiles::http::route_for_request(&*self.routes, req).ok_or(NoRoute)
    }
}
