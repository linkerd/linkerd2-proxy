//! A stack that routes HTTP requests to concrete backends.

use super::concrete;
use crate::{BackendRef, EndpointRef, Outbound, OutboundMetrics, ParentRef};
use linkerd_app_core::{
    proxy::{api_resolve::Metadata, http},
    svc,
    transport::addrs::*,
    Addr, Error, Infallible, NameAddr, CANONICAL_DST_HEADER,
};
use std::{fmt::Debug, hash::Hash, sync::Arc};
use tokio::sync::watch;

pub mod policy;
pub mod profile;

#[cfg(test)]
mod tests;

/// Indicates the address used for logical routing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogicalAddr(pub Addr);

/// Configures the flavor of HTTP routing.
#[derive(Clone, Debug, PartialEq)]
pub enum Routes {
    /// Policy routes.
    Policy(policy::Params),

    /// Service profile routes.
    Profile(profile::Routes),

    /// Fallback endpoint forwarding.
    // TODO(ver) Remove this variant when policy routes are fully wired up.
    Endpoint(Remote<ServerAddr>, Arc<Metadata>),
}

#[derive(Clone, Debug)]
pub struct Concrete<T> {
    target: concrete::Dispatch,
    authority: Option<http::uri::Authority>,
    parent: T,
    parent_ref: ParentRef,
    backend_ref: BackendRef,
    failure_accrual: Option<policy::FailureAccrual>,
    retry_after: Option<policy::RetryAfterConfig>,
}

impl<T: PartialEq> PartialEq for Concrete<T> {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.authority == other.authority
            && self.parent == other.parent
            && self.parent_ref == other.parent_ref
            && self.backend_ref == other.backend_ref
        // failure_accrual and retry_after are left out on purpose.
        // They configure the breaker but do not determine backend
        // identity, and the backend cache is rebuilt whenever a policy
        // update changes them, so they need not take part in the key.
    }
}

impl<T: Eq> Eq for Concrete<T> {}

impl<T: Hash> Hash for Concrete<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.authority.hash(state);
        self.parent.hash(state);
        self.parent_ref.hash(state);
        self.backend_ref.hash(state);
        // failure_accrual and retry_after are left out on purpose,
        // matching the equality impl. They do not determine identity,
        // and SuccessRateConfig contains an f64 that does not impl Hash.
    }
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

#[derive(Clone, Debug, PartialEq)]
enum RouterParams<T: Clone + Debug + Eq + Hash> {
    Policy(policy::Policy<T>),

    Profile(profile::Params<T>),

    // TODO(ver) Remove this variant when policy routes are fully wired up.
    Endpoint(Remote<ServerAddr>, Arc<Metadata>, T),
}

// Only applies to requests with profiles.
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
    pub fn push_http_logical<T, NSvc>(self) -> Outbound<svc::ArcNewCloneHttp<T>>
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
            concrete
                // Share the concrete stack with each router stack.
                .lift_new()
                .push_on_service(RouterParams::layer(rt.metrics.clone()))
                // Rebuild the inner router stack every time the watch changes.
                .push(svc::NewSpawnWatch::<Routes, _>::layer_into::<RouterParams<T>>())
                .arc_new_clone_http()
        })
    }
}

// === impl RouterParams ===

impl<T> RouterParams<T>
where
    T: Clone + Debug + Eq + Hash + Send + Sync + 'static,
{
    fn layer<N, S>(
        metrics: OutboundMetrics,
    ) -> impl svc::Layer<N, Service = svc::ArcNewCloneHttp<RouterParams<T>>> + Clone
    where
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
            let policy = svc::stack(concrete.clone()).push(policy::Policy::layer(
                metrics.prom.http.http_route.clone(),
                metrics.prom.http.grpc_route.clone(),
            ));
            let profile =
                svc::stack(concrete.clone()).push(profile::Params::layer(metrics.proxy.clone()));
            svc::stack(concrete)
                .push_switch(
                    |prms: Self| {
                        Ok::<_, Infallible>(match prms {
                            Self::Endpoint(remote, meta, parent) => {
                                let ep = EndpointRef::new(
                                    &meta,
                                    remote.port().try_into().expect("port must not be 0"),
                                );
                                let parent_ref = ParentRef::from(ep.clone());
                                let backend_ref = BackendRef::from(ep);
                                svc::Either::Left(Concrete {
                                    parent_ref,
                                    backend_ref,
                                    target: concrete::Dispatch::Forward(remote, meta),
                                    authority: None,
                                    parent,
                                    failure_accrual: None,
                                    retry_after: None,
                                })
                            }
                            Self::Profile(profile) => {
                                svc::Either::Right(svc::Either::Left(profile))
                            }
                            Self::Policy(policy) => svc::Either::Right(svc::Either::Right(policy)),
                        })
                    },
                    // Switch profile and policy routing.
                    profile
                        .push_switch(Ok::<_, Infallible>, policy.into_inner())
                        .into_inner(),
                )
                .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                .arc_new_clone_http()
                .into_inner()
        })
    }
}

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

impl<T> svc::Param<http::Variant> for Concrete<T>
where
    T: svc::Param<http::Variant>,
{
    fn param(&self) -> http::Variant {
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

impl<T> svc::Param<ParentRef> for Concrete<T> {
    fn param(&self) -> ParentRef {
        self.parent_ref.clone()
    }
}

impl<T> svc::Param<BackendRef> for Concrete<T> {
    fn param(&self) -> BackendRef {
        self.backend_ref.clone()
    }
}

impl<T> svc::Param<Option<policy::FailureAccrual>> for Concrete<T> {
    fn param(&self) -> Option<policy::FailureAccrual> {
        self.failure_accrual.clone()
    }
}

impl<T> svc::Param<Option<policy::RetryAfterConfig>> for Concrete<T> {
    fn param(&self) -> Option<policy::RetryAfterConfig> {
        self.retry_after
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

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::http::concrete;
    use linkerd_app_core::{exp_backoff::ExponentialBackoff, NameAddr};
    use linkerd_proxy_client_policy::Meta;
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
        time::Duration,
    };

    fn hash_of<T: Hash>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    // Builds a `Concrete<()>` whose five identity fields are fixed, leaving
    // the breaker configuration to the caller. The identity fields decide
    // which backend the cache key selects.
    fn base(
        failure_accrual: Option<policy::FailureAccrual>,
        retry_after: Option<policy::RetryAfterConfig>,
    ) -> Concrete<()> {
        let addr: NameAddr = "example.com:1234".parse().unwrap();
        let meta = Meta::new_default("test");
        Concrete {
            target: concrete::Dispatch::Balance(addr.clone(), super::profile::DEFAULT_EWMA),
            authority: Some(addr.as_http_authority()),
            parent: (),
            parent_ref: ParentRef(meta.clone()),
            backend_ref: BackendRef(meta),
            failure_accrual,
            retry_after,
        }
    }

    fn some_accrual() -> policy::FailureAccrual {
        policy::FailureAccrual {
            consecutive: policy::ConsecutiveFailures {
                max_failures: 3,
                backoff: ExponentialBackoff::new_unchecked(
                    Duration::from_secs(1),
                    Duration::from_secs(60),
                    0.0,
                ),
            },
            success_rate: None,
        }
    }

    // The concrete target keeps breaker configuration out of its cache key on
    // purpose, so two concretes that differ only in `failure_accrual` must
    // compare equal and hash equal. Otherwise a config-only change would rebuild
    // the backend cache.
    #[test]
    fn failure_accrual_does_not_affect_identity() {
        let without = base(None, None);
        let with = base(Some(some_accrual()), None);

        assert_eq!(without, with);
        assert_eq!(hash_of(&without), hash_of(&with));
    }

    // `retry_after` is also kept out of the cache key, so toggling it
    // must not change a concrete's identity.
    #[test]
    fn retry_after_does_not_affect_identity() {
        let without = base(None, None);
        let with = base(
            None,
            Some(policy::RetryAfterConfig {
                max_duration: Duration::from_secs(30),
            }),
        );

        assert_eq!(without, with);
        assert_eq!(hash_of(&without), hash_of(&with));
    }

    // Both breaker fields are kept out together. A concrete that holds full
    // breaker configuration can be swapped with one that holds none.
    #[test]
    fn breaker_config_does_not_affect_identity() {
        let without = base(None, None);
        let with = base(
            Some(some_accrual()),
            Some(policy::RetryAfterConfig {
                max_duration: Duration::from_secs(30),
            }),
        );

        assert_eq!(without, with);
        assert_eq!(hash_of(&without), hash_of(&with));

        // Check that an identity field still tells concretes apart, so
        // equality is not true for an empty reason.
        let other_addr: NameAddr = "other.example.com:1234".parse().unwrap();
        let mut different = base(None, None);
        different.target = concrete::Dispatch::Balance(other_addr, super::profile::DEFAULT_EWMA);
        assert_ne!(without, different);
    }
}
