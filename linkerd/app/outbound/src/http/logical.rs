//! A stack that routes HTTP requests to concrete backends.

use super::concrete;
use crate::Outbound;
use linkerd_app_core::{
    metrics,
    proxy::{api_resolve::Metadata, http},
    svc,
    transport::addrs::*,
    Addr, Error, Infallible, NameAddr, CANONICAL_DST_HEADER,
};
use linkerd_distribute as distribute;
use std::{fmt::Debug, hash::Hash};
use tokio::sync::watch;

pub mod policy;
pub mod profile;

pub use self::profile::Routes as ProfileRoutes;

/// Indicates the address used for logical routing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogicalAddr(pub Addr);

/// Configures the flavor of HTTP routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routes {
    /// Policy routes.
    Policy(policy::Routes),

    /// Service profile routes.
    Profile(profile::Routes),

    /// Fallback endpoint forwarding.
    // TODO(ver) Remove this variant when policy routes are fully wired up.
    Endpoint(Remote<ServerAddr>, Metadata),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete<T> {
    target: concrete::Dispatch,
    authority: Option<http::uri::Authority>,
    parent: T,
}

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Debug, thiserror::Error)]
#[error("logical service {addr}: {source}")]
pub struct LogicalError {
    addr: Addr,
    #[source]
    source: Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RouterParams<T: Clone + Debug + Eq + Hash> {
    Policy(policy::Params<T>),

    Profile(profile::Params<T>),

    // TODO(ver) Remove this variant when policy routes are fully wired up.
    Endpoint(Remote<ServerAddr>, Metadata, T),
}

type BackendCache<T, N, S> = distribute::BackendCache<Concrete<T>, N, S>;
type Distribution<T> = distribute::Distribution<Concrete<T>>;

// Only applies to requests with profiles.
//
// TODO Add l5d-dst-canonical header to requests.
//
// TODO(ver) move this into the endpoint stack so that we can only
// set this on meshed connections.
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
        T: svc::Param<watch::Receiver<Routes>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Concrete stack.
        N: svc::NewService<Concrete<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Clone + Send + Sync + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|_config, rt, concrete| {
            // For each `T` target, watch its `Profile`, rebuilding a
            // router stack.
            let watch = concrete
                // Share the concrete stack with each router stack.
                .lift_new()
                .push_on_service(router_layer(rt.metrics.proxy.clone()))
                // Rebuild the inner router stack every time the watch changes.
                .push(svc::NewSpawnWatch::<Routes, _>::layer_into::<RouterParams<T>>());

            watch
                .push_on_service(svc::MapErr::layer_boxed())
                .push(svc::ArcNewService::layer())
        })
    }
}

fn router_layer<T, N, S>(
    metrics: metrics::Proxy,
) -> impl svc::Layer<
    N,
    Service = svc::ArcNewService<
        RouterParams<T>,
        impl svc::Service<
                http::Request<http::BoxBody>,
                Response = http::Response<http::BoxBody>,
                Error = Error,
                Future = impl Send,
            > + Clone,
    >,
> + Clone
where
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
    N: svc::NewService<Concrete<T>, Service = S>,
    N: Clone + Send + Sync + 'static,
    S: svc::Service<
        http::Request<http::BoxBody>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
    >,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
{
    svc::layer::mk(move |concrete: N| {
        let profile = svc::stack(concrete.clone()).push(profile::layer(metrics.clone()));
        let policy = svc::stack(concrete.clone()).push(policy::layer());
        svc::stack(concrete)
            .push_switch(
                |prms: RouterParams<T>| {
                    Ok::<_, Infallible>(match prms {
                        RouterParams::Endpoint(remote, meta, parent) => svc::Either::A(Concrete {
                            target: concrete::Dispatch::Forward(remote, meta),
                            authority: None,
                            parent,
                        }),
                        RouterParams::Profile(profile) => svc::Either::B(svc::Either::A(profile)),
                        RouterParams::Policy(policy) => svc::Either::B(svc::Either::B(policy)),
                    })
                },
                // Switch profile and policy routing.
                profile
                    .push_switch(Ok::<_, Infallible>, policy.into_inner())
                    .into_inner(),
            )
            .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
            .push_on_service(svc::MapErr::layer_boxed())
            .push(svc::ArcNewService::layer())
            .into_inner()
    })
}

// === impl RouterParams ===

impl<T> From<(Routes, T)> for RouterParams<T>
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((routes, parent): (Routes, T)) -> Self {
        match routes {
            Routes::Policy(routes) => Self::Policy((routes, parent).into()),
            Routes::Profile(routes) => Self::Profile((routes, parent).into()),
            Routes::Endpoint(addr, metadata) => Self::Endpoint(addr, metadata, parent),
        }
    }
}

impl<T> svc::Param<LogicalAddr> for RouterParams<T>
where
    T: Clone + Debug + Eq + Hash,
{
    fn param(&self) -> LogicalAddr {
        match self {
            Self::Policy(ref p) => p.param(),
            Self::Profile(ref p) => {
                let profile::LogicalAddr(addr) = p.param();
                LogicalAddr(addr.into())
            }
            Self::Endpoint(Remote(ServerAddr(ref addr)), ..) => LogicalAddr((*addr).into()),
        }
    }
}

// === impl LogicalError ===

impl<T> From<(&RouterParams<T>, Error)> for LogicalError
where
    T: Eq + Hash + Clone + Debug,
{
    fn from((target, source): (&RouterParams<T>, Error)) -> Self {
        let LogicalAddr(addr) = svc::Param::param(target);
        Self { addr, source }
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

impl<T> svc::Param<Option<http::uri::Authority>> for Concrete<T> {
    fn param(&self) -> Option<http::uri::Authority> {
        self.authority.clone()
    }
}

impl<T> svc::Param<concrete::Dispatch> for Concrete<T> {
    fn param(&self) -> concrete::Dispatch {
        self.target.clone()
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
