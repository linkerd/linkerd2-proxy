use super::{
    super::{concrete, Concrete},
    route, LogicalAddr, NoRoute,
};
use crate::{BackendRef, EndpointRef, RouteRef};
use linkerd_app_core::{io, proxy::http, svc, transport::addrs::*, Addr, Error, NameAddr, Result};
use linkerd_distribute as distribute;
use linkerd_opaq_route as opaq_route;
use linkerd_proxy_client_policy as policy;
use std::{fmt::Debug, hash::Hash, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Router<T: Clone + Debug + Eq + Hash> {
    pub(super) parent: T,
    pub(super) addr: Addr,
    pub(super) routes: Arc<[opaq_route::Route<route::Route<T>>]>,
    pub(super) backends: distribute::Backends<Concrete<T>>,
}

type NewBackendCache<T, N, S> = distribute::NewBackendCache<Concrete<T>, (), N, S>;

// === impl Router ===
impl<T> Router<T>
where
    // Parent target type.
    T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
{
    pub fn layer<N, I, NSvc>() -> impl svc::Layer<N, Service = svc::ArcNewCloneTcp<Self, I>> + Clone
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
                .push_on_service(route::MatchedRoute::layer())
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
            addr,
            meta: parent_ref,
            routes,
            backends,
        } = rts;

        let mk_concrete = {
            let parent = parent.clone();
            let parent_ref = parent_ref.clone();

            move |backend_ref: BackendRef, target: concrete::Dispatch| Concrete {
                target,
                parent: parent.clone(),
                backend_ref,
                parent_ref: parent_ref.clone(),
            }
        };

        let mk_dispatch = move |bke: &policy::Backend, addr: Addr| match bke.dispatcher {
            policy::BackendDispatcher::BalanceP2c(
                policy::Load::PeakEwma(policy::PeakEwma { decay, default_rtt }),
                policy::EndpointDiscovery::DestinationGet { ref path },
            ) => mk_concrete(
                BackendRef(bke.meta.clone()),
                concrete::Dispatch::Balance(
                    addr,
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
            |route_ref: &RouteRef, rb: &policy::RouteBackend<policy::opaq::Filter>, addr: Addr| {
                let concrete = mk_dispatch(&rb.backend, addr);
                route::Backend {
                    route_ref: route_ref.clone(),
                    concrete,
                }
            };

        let mk_distribution = |rr: &RouteRef,
                               d: &policy::RouteDistribution<policy::opaq::Filter>,
                               addr: Addr| match d {
            policy::RouteDistribution::Empty => route::BackendDistribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                route::BackendDistribution::first_available(
                    backends
                        .iter()
                        .map(|b| mk_route_backend(rr, b, addr.clone())),
                )
            }
            policy::RouteDistribution::RandomAvailable(backends) => {
                route::BackendDistribution::random_available(
                    backends
                        .iter()
                        .map(|(rb, weight)| (mk_route_backend(rr, rb, addr.clone()), *weight)),
                )
                .expect("distribution must be valid")
            }
        };

        let mk_policy =
            |policy::RoutePolicy::<policy::opaq::Filter, ()> {
                 meta, distribution, ..
             },
             addr: Addr,
             forbidden: bool| {
                let route_ref = RouteRef(meta);
                let parent_ref = parent_ref.clone();

                let distribution = mk_distribution(&route_ref, &distribution, addr.clone());
                route::Route {
                    addr,
                    parent: parent.clone(),
                    parent_ref: parent_ref.clone(),
                    route_ref,
                    distribution,
                    forbidden,
                }
            };

        let routes = routes
            .iter()
            .map(|route| opaq_route::Route {
                policy: mk_policy(route.policy.clone(), addr.clone(), route.forbidden),
                forbidden: route.forbidden,
            })
            .collect();

        let backends = backends
            .iter()
            .map(|b| mk_dispatch(b, addr.clone()))
            .collect();

        Self {
            routes,
            backends,
            addr,
            parent,
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
        let (r#match, params) = policy::opaq::find(&self.routes).ok_or(NoRoute)?;
        tracing::debug!(meta = ?params.route_ref, "Selected route");
        tracing::trace!(?r#match);

        Ok(route::MatchedRoute {
            r#match,
            params: params.clone(),
        })
    }
}

impl<T> svc::Param<LogicalAddr> for Router<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> LogicalAddr {
        LogicalAddr(self.addr.clone())
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
