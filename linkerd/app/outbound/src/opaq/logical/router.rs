use super::{
    super::{concrete, Concrete},
    route, Logical, NoRoute,
};
use crate::{BackendRef, EndpointRef, RouteRef, ServerAddr};
use linkerd_app_core::{io, proxy::http, svc, transport::addrs::*, Error, NameAddr, Result};
use linkerd_distribute as distribute;
use linkerd_opaq_route as opaq_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Router<T: Clone + Debug + Eq + Hash> {
    pub(super) parent: T,
    pub(super) logical: Logical,
    pub(super) routes: Option<opaq_route::Route<route::Route<T>>>,
    pub(super) backends: distribute::Backends<Concrete<T>>,
}

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Router ===
impl<T> Router<T>
where
    // Parent target type.
    T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
{
    pub fn layer<N, I, NSvc>(
        metrics: route::TcpRouteMetrics,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneTcp<Self, I>> + Clone
    where
        I: io::AsyncRead + io::AsyncWrite + Debug + Send + Unpin + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = ()> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        NSvc::Error: Into<Error>,
    {
        svc::layer::mk(move |inner| {
            svc::stack(inner)
                .lift_new()
                // Each route builds over concrete backends. All of these
                // backends are cached here and shared across routes.
                .push(NewBackendCache::layer())
                .push_on_service(route::MatchedRoute::layer(metrics.clone()))
                .push(svc::NewOneshotRoute::<Self, (), _>::layer_cached())
                .arc_new_clone_tcp()
                .into_inner()
        })
    }
}

impl<T> From<(crate::opaq::Routes, T)> for Router<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((rts, parent): (crate::opaq::Routes, T)) -> Self {
        let crate::opaq::Routes {
            logical,
            routes,
            backends,
        } = rts;

        let mk_concrete = {
            let parent = parent.clone();
            let logical = logical.clone();

            move |backend_ref: BackendRef, target: concrete::Dispatch| Concrete {
                target,
                parent: parent.clone(),
                backend_ref,
                logical: logical.clone(),
            }
        };

        let mk_dispatch = move |bke: &policy::Backend| match bke.dispatcher {
            policy::BackendDispatcher::BalanceP2c(
                policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                policy::EndpointDiscovery::DestinationGet { ref path },
            ) => mk_concrete(
                BackendRef(bke.meta.clone()),
                concrete::Dispatch::Balance(
                    path.parse::<NameAddr>()
                        .expect("destination must be a nameaddr"),
                    http::balance::EwmaConfig { decay, default_rtt },
                ),
            ),
            policy::BackendDispatcher::Forward(addr, ref md) => mk_concrete(
                EndpointRef::new(md, addr.port().try_into().expect("port must not be 0")).into(),
                concrete::Dispatch::Forward(Remote(ServerAddr(addr)), md.clone()),
            ),
            policy::BackendDispatcher::Fail { ref message } => mk_concrete(
                BackendRef(policy::Meta::new_default("fail")),
                concrete::Dispatch::Fail {
                    message: message.clone(),
                },
            ),
        };

        let mk_route_backend =
            |route_ref: &RouteRef, rb: &policy::RouteBackend<policy::opaq::Filter>| {
                let concrete = mk_dispatch(&rb.backend);
                route::Backend {
                    route_ref: route_ref.clone(),
                    concrete,
                }
            };

        let mk_distribution =
            |rr: &RouteRef, d: &policy::RouteDistribution<policy::opaq::Filter>| match d {
                policy::RouteDistribution::Empty => route::BackendDistribution::Empty,
                policy::RouteDistribution::FirstAvailable(backends) => {
                    route::BackendDistribution::first_available(
                        backends.iter().map(|b| mk_route_backend(rr, b)),
                    )
                }
                policy::RouteDistribution::RandomAvailable(backends) => {
                    route::BackendDistribution::random_available(
                        backends
                            .iter()
                            .map(|(rb, weight)| (mk_route_backend(rr, rb), *weight)),
                    )
                    .expect("distribution must be valid")
                }
            };

        let mk_policy = |policy::RoutePolicy::<policy::opaq::Filter, ()> {
                             meta,
                             distribution,
                             filters: _,
                             params: (),
                         }| {
            let route_ref = RouteRef(meta);
            let logical = logical.clone();

            let distribution = mk_distribution(&route_ref, &distribution);
            route::Route {
                logical,
                parent: parent.clone(),
                route_ref,
                distribution,
                forbidden: false, // TODO
            }
        };

        let routes = routes.as_ref().map(|route| opaq_route::Route {
            policy: mk_policy(route.policy.clone()),
        });

        let backends = backends.iter().map(mk_dispatch).collect();

        Self {
            routes,
            backends,
            parent,
            logical,
        }
    }
}

impl<T, I> svc::router::SelectRoute<I> for Router<T>
where
    T: Clone + Eq + Hash + Debug,
{
    type Key = route::MatchedRoute<T>;
    type Error = NoRoute;

    fn select(&self, _: &I) -> Result<Self::Key, Self::Error> {
        tracing::trace!("Selecting Opaq route");
        let Some(ref route) = self.routes else {
            return Err(NoRoute);
        };
        let params = route.policy.clone();
        tracing::debug!(meta = ?params.route_ref, "Selected route");

        Ok(route::MatchedRoute { params })
    }
}

impl<T> svc::Param<Logical> for Router<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> Logical {
        self.logical.clone()
    }
}

impl<T> svc::Param<distribute::Backends<Concrete<T>>> for Router<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}
