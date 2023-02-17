//! A stack that routes HTTP requests to concrete backends.

use super::{concrete, retry};
use crate::Outbound;
use linkerd_app_core::{
    classify, metrics,
    profiles::{self, Profile},
    proxy::{
        api_resolve::Metadata,
        http::{self, balance},
    },
    svc,
    transport::addrs::*,
    Error, Infallible, NameAddr, CANONICAL_DST_HEADER,
};
use linkerd_distribute as distribute;
use linkerd_proxy_client_policy::ClientPolicy;
use std::{fmt::Debug, hash::Hash, sync::Arc, time};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub enum Logical {
    Route(NameAddr, profiles::Receiver, watch::Receiver<ClientPolicy>),
    Forward(Remote<ServerAddr>, Metadata),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    target: concrete::Dispatch,
    parent: T,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {addr}: {source}")]
pub struct LogicalError {
    addr: NameAddr,
    #[source]
    source: Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params<T: Clone + Debug + Eq + Hash> {
    parent: T,
    addr: NameAddr,
    routes: Arc<[(profiles::http::RequestMatch, RouteParams<T>)]>,
    backends: distribute::Backends<Concrete<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams<T> {
    parent: T,
    addr: NameAddr,
    profile: profiles::http::Route,
    distribution: Distribution<T>,
}

type BackendCache<T, N, S> = distribute::BackendCache<Concrete<T>, N, S>;
type Distribution<T> = distribute::Distribution<Concrete<T>>;

#[derive(Clone, Debug)]
struct Routable<T> {
    parent: T,
    addr: NameAddr,
    profile: profiles::Receiver,
}

#[derive(Clone, Debug)]
struct CanonicalDstHeader(NameAddr);

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a `NewService` that produces a router service for each logical
    /// target.
    ///
    /// The router uses discovery information (provided on the target) to
    /// support per-request routing over a set of concrete inner services.
    /// Only available inner services are used for routing. When there are no
    /// available backends, requests are failed with a [`svc::stack::LoadShedError`].
    pub fn push_http_logical<T, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        // Logical target.
        T: svc::Param<Logical>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
    {
        self.map_stack(|_, rt, concrete| {
            let route = svc::layers()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        // The router does not take the backend's availability into
                        // consideration, so we must eagerly fail requests to prevent
                        // leaking tasks onto the runtime.
                        .push(svc::LoadShed::layer()),
                )
                .push(http::insert::NewInsert::<RouteParams<T>, _>::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_profile_route_actual
                        .to_layer::<classify::Response, _, RouteParams<T>>(),
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
                        .to_layer::<classify::Response, _, RouteParams<T>>(),
                )
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                // TODO(ver) .push(svc::NewMapErr::layer_from_target::<RouteError, _>())
                .push_on_service(http::BoxResponse::layer());

            // A `NewService`--instantiated once per logical target--that caches
            // a set of concrete services so that, as the watch provides new
            // `Params`, we can reuse inner services.
            let router = svc::layers()
                // Each `RouteParams` provides a `Distribution` that is used to
                // choose a concrete service for a given route.
                .push(BackendCache::layer())
                // Lazily cache a service for each `RouteParams`
                // returned from the `SelectRoute` impl.
                .push_on_service(route)
                .push(svc::NewOneshotRoute::<Params<T>, _, _>::layer_cached());

            // For each `T` target, watch its `Profile`, rebuilding a
            // router stack.
            let watch = concrete
                .clone()
                // Share the concrete stack with each router stack.
                .lift_new()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                .push(svc::NewSpawnWatch::<Profile, _>::layer_into::<Params<T>>());

            watch
                // Add l5d-dst-canonical header to requests.
                //
                // TODO(ver) move this into the endpoint stack so that we can only
                // set this on meshed connections.
                //
                // TODO(ver) do we need to strip headers here?
                .push(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        Ok(match parent.param() {
                            Logical::Route(addr, profile, _policy) => svc::Either::A(Routable {
                                addr,
                                parent,
                                profile,
                            }),
                            Logical::Forward(addr, meta) => svc::Either::B(Concrete {
                                target: concrete::Dispatch::Forward(addr, meta),
                                parent,
                            }),
                        })
                    },
                    concrete.into_inner(),
                )
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Routable ===

impl<T> svc::Param<watch::Receiver<profiles::Profile>> for Routable<T> {
    fn param(&self) -> watch::Receiver<profiles::Profile> {
        self.profile.clone().into()
    }
}

impl<T> svc::Param<CanonicalDstHeader> for Routable<T> {
    fn param(&self) -> CanonicalDstHeader {
        CanonicalDstHeader(self.addr.clone())
    }
}

// === impl Params ===

impl<T> From<(Profile, Routable<T>)> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((profile, routable): (Profile, Routable<T>)) -> Self {
        const EWMA: balance::EwmaConfig = balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        };

        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let concrete = Concrete {
                target: concrete::Dispatch::Balance(routable.addr.clone(), EWMA),
                parent: routable.parent.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = profile
                .targets
                .iter()
                .map(|t| Concrete {
                    target: concrete::Dispatch::Balance(t.addr.clone(), EWMA),
                    parent: routable.parent.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        target: concrete::Dispatch::Balance(addr, EWMA),
                        parent: routable.parent.clone(),
                    };
                    (concrete, weight)
                },
            ))
            .expect("distribution must be valid");

            (backends, distribution)
        };

        let routes = profile
            .http_routes
            .iter()
            .cloned()
            .map(|(req_match, profile)| {
                let params = RouteParams {
                    addr: routable.addr.clone(),
                    profile,
                    parent: routable.parent.clone(),
                    distribution: distribution.clone(),
                };
                (req_match, params)
            })
            // Add a default route.
            .chain(std::iter::once((
                profiles::http::RequestMatch::default(),
                RouteParams {
                    addr: routable.addr.clone(),
                    profile: Default::default(),
                    parent: routable.parent.clone(),
                    distribution: distribution.clone(),
                },
            )))
            .collect::<Arc<[(_, _)]>>();

        Self {
            addr: routable.addr,
            parent: routable.parent,
            backends,
            routes,
        }
    }
}

impl<T> svc::Param<distribute::Backends<Concrete<T>>> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> distribute::Backends<Concrete<T>> {
        self.backends.clone()
    }
}

impl<T> svc::Param<profiles::LogicalAddr> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.addr.clone())
    }
}

impl<T, B> svc::router::SelectRoute<http::Request<B>> for Params<T>
where
    T: Eq + Hash + Clone + Debug,
{
    type Key = RouteParams<T>;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        profiles::http::route_for_request(&*self.routes, req)
            .ok_or(NoRoute)
            .cloned()
    }
}

// === impl RouteParams ===

impl<T> svc::Param<profiles::LogicalAddr> for RouteParams<T> {
    fn param(&self) -> profiles::LogicalAddr {
        profiles::LogicalAddr(self.addr.clone())
    }
}

impl<T: Clone> svc::Param<Distribution<T>> for RouteParams<T> {
    fn param(&self) -> Distribution<T> {
        self.distribution.clone()
    }
}

impl<T> svc::Param<profiles::http::Route> for RouteParams<T> {
    fn param(&self) -> profiles::http::Route {
        self.profile.clone()
    }
}

impl<T> svc::Param<metrics::ProfileRouteLabels> for RouteParams<T> {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::outbound(
            profiles::LogicalAddr(self.addr.clone()),
            &self.profile,
        )
    }
}

impl<T> svc::Param<http::ResponseTimeout> for RouteParams<T> {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.profile.timeout())
    }
}

impl<T> classify::CanClassify for RouteParams<T> {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.profile.response_classes().clone().into()
    }
}

// === impl LogicalError ===

impl<T> From<(&Routable<T>, Error)> for LogicalError {
    fn from((target, source): (&Routable<T>, Error)) -> Self {
        Self {
            addr: target.addr.clone(),
            source,
        }
    }
}

// === impl Concrete ===

impl<T> svc::Param<http::Version> for Concrete<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> http::Version {
        self.parent.param()
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Concrete<T>
where
    T: svc::Param<Option<profiles::LogicalAddr>>,
{
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.parent.param()
    }
}

impl<T> svc::Param<concrete::Dispatch> for Concrete<T> {
    fn param(&self) -> concrete::Dispatch {
        self.target.clone()
    }
}

// === impl Logical ===

impl std::cmp::PartialEq for Logical {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Route(laddr, _, _), Self::Route(raddr, _, _)) => laddr == raddr,
            (Self::Forward(laddr, lmeta), Self::Forward(raddr, rmeta)) => {
                laddr == raddr && lmeta == rmeta
            }
            _ => false,
        }
    }
}

impl std::cmp::Eq for Logical {}

impl std::hash::Hash for Logical {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Route(addr, _, _) => {
                addr.hash(state);
            }
            Self::Forward(addr, meta) => {
                addr.hash(state);
                meta.hash(state);
            }
        }
    }
}

// === impl CanonicalDstHeader ===

impl From<CanonicalDstHeader> for http::HeaderPair {
    fn from(CanonicalDstHeader(dst): CanonicalDstHeader) -> http::HeaderPair {
        http::HeaderPair(
            http::HeaderName::from_static(CANONICAL_DST_HEADER),
            http::HeaderValue::from_str(&dst.to_string()).expect("addr must be a valid header"),
        )
    }
}
