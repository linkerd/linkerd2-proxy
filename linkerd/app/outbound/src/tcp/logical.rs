#[cfg(test)]
mod tests;

use super::{Concrete, Endpoint, Logical};
use crate::{endpoint, resolve, Outbound};
use linkerd_app_core::{
    drain, io,
    profiles::{self, Profile},
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        resolve::map_endpoint,
        tcp,
    },
    svc, Error, Infallible,
};
use linkerd_distribute as distribute;
use linkerd_router as router;
use tracing::debug_span;

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    logical: Logical,
    route: RouteParams,
    keys: router::RouteKeys<RouteParams>,
    backends: distribute::Backends<Concrete>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams {
    logical: Logical,
    profile: profiles::tcp::Route,
    distribution: Distribution,
}

type CacheNewDistribute<N, S> = distribute::CacheNewDistribute<Concrete, N, S>;
type Distribution = distribute::Distribution<Concrete>;

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Constructs a TCP load balancer.
    pub fn push_tcp_concrete<I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Concrete,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        C: svc::MakeConnection<Endpoint> + Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
        C: Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, connect| {
            let crate::Config {
                discovery_idle_timeout,
                tcp_connection_buffer,
                ..
            } = config;

            let resolve = svc::stack(resolve.into_service())
                .check_service::<ConcreteAddr>()
                .push_request_filter(|c: Concrete| Ok::<_, Infallible>(c.resolve))
                .push(svc::layer::mk(move |inner| {
                    map_endpoint::Resolve::new(
                        endpoint::FromMetadata {
                            inbound_ips: config.inbound_ips.clone(),
                        },
                        inner,
                    )
                }))
                .check_service::<Concrete>()
                .into_inner();

            connect
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_make_thunk()
                .instrument(|t: &Endpoint| debug_span!("endpoint", addr = %t.addr))
                .push(resolve::layer(resolve, *discovery_idle_timeout * 2))
                .push_on_service(
                    svc::layers()
                        .push(tcp::balance::layer(
                            crate::EWMA_DEFAULT_RTT,
                            crate::EWMA_DECAY,
                        ))
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .into_new_service()
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(crate::stack_labels("tcp", "concrete")),
                        )
                        .push_buffer("TCP Concrete", tcp_connection_buffer),
                )
                .check_new_service::<Concrete, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}

impl<N> Outbound<N> {
    pub fn push_tcp_logical<I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Clone + Send + Sync + 'static,
        NSvc::Future: Send,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
    {
        self.map_stack(|config, _, concrete| {
            let crate::Config {
                discovery_idle_timeout,
                ..
            } = config;

            let route = svc::layers();

            concrete
                .check_new_service::<Concrete, I>()
                // Drop the `Logical` target type and clone the inner concrete
                // stack into the distributor.
                .push(svc::layer::mk(svc::NewCloneService::from))
                .push_on_service(
                    svc::layers()
                        .push(CacheNewDistribute::layer())
                        .push(router::NewRoute::<RouteParams, _, _>::layer(route)),
                )
                .check_new_new::<Logical, Params>()
                .push(svc::NewWatchInto::<Params, _>::layer())
                .check_new_new::<Logical, Profile>()
                .push(svc::NewSpawnWatch::<Profile, _>::layer())
                .check_new_service::<Logical, I>()
                // This caches each logical stack so that it can be reused
                // across per-connection server stacks (i.e., created by the
                // DetectService).
                //
                // TODO(ver) Update the detection stack so this dynamic caching
                // can be removed.
                .push_cache(*discovery_idle_timeout)
                .instrument(|_: &Logical| debug_span!("tcp"))
                .check_new_service::<Logical, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl From<(Profile, Logical)> for Params {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's
        let backends = profile
            .target_addrs
            .iter()
            .map(|addr| super::Concrete {
                resolve: ConcreteAddr(addr.clone()),
                logical: logical.clone(),
            })
            .collect();

        let route = {
            let distribution =
                Distribution::random_available(profile.tcp_route.targets().iter().cloned().map(
                    |profiles::Target { addr, weight }| {
                        let concrete = Concrete {
                            logical: logical.clone(),
                            resolve: ConcreteAddr(addr),
                        };
                        (concrete, weight)
                    },
                ))
                .expect("distribution must be valid");
            RouteParams {
                logical: logical.clone(),
                distribution,
                profile: profile.tcp_route,
            }
        };
        let keys = std::iter::once(route.clone()).collect();

        Self {
            logical,
            backends,
            route,
            keys,
        }
    }
}

// === impl Params ===

impl svc::Param<router::RouteKeys<RouteParams>> for Params {
    fn param(&self) -> router::RouteKeys<RouteParams> {
        self.keys.clone()
    }
}

impl svc::Param<distribute::Backends<Concrete>> for Params {
    fn param(&self) -> distribute::Backends<Concrete> {
        self.backends.clone()
    }
}

impl svc::Param<profiles::LogicalAddr> for Params {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}

impl<I> router::SelectRoute<I> for Params {
    type Key = RouteParams;
    type Error = std::convert::Infallible;

    fn select<'r>(&self, _: &'r I) -> Result<&Self::Key, Self::Error> {
        Ok(&self.route)
    }
}

// === impl RouteParams ===

impl svc::Param<Distribution> for RouteParams {
    fn param(&self) -> Distribution {
        self.distribution.clone()
    }
}
