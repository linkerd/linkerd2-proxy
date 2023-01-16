#![allow(warnings)]

use super::{retry, CanonicalDstHeader};
use crate::Outbound;
use linkerd_app_core::{
    classify, metrics,
    profiles::{self, Profile},
    proxy::http,
    svc, Error,
};
use linkerd_distribute as distribute;
use linkerd_proxy_client_policy::{self as policy, ClientPolicy};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, thiserror::Error)]
#[error("no route")]
pub struct NoRoute(());

#[derive(Clone, Debug, PartialEq, Eq)]
struct Params {
    // routes: Arc<[(profiles::http::RequestMatch, RouteParams)]>,
    // profile: Option<discover::LogicalProfile>,
    policy: ClientPolicy,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteParams {
    // profile: profiles::http::Route,
    policy: policy::http::Policy,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Concrete {
    backend: policy::Backend,
    version: http::Version,
}

type BackendCache<N, S> = distribute::BackendCache<Concrete, N, S>;
type Distribution = distribute::Distribution<Concrete>;

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a `NewService` that produces a router service for each logical
    /// target.
    ///
    /// The router uses discovery information (provided on the target) to
    /// support per-request routing over a set of concrete inner services.
    /// Only available inner services are used for routing. When there are no
    /// available backends, requests are failed with a [`svc::stack::LoadShedError`].
    ///
    // TODO(ver) make the outer target type generic/parameterized.
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
        T: svc::Param<watch::Receiver<ClientPolicy>>,
        T: svc::Param<http::Version>,
        T: Clone + Send + Sync + 'static,
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
                // .push(http::insert::NewInsert::<RouteParams, _>::layer())
                // .push(
                //     rt.metrics
                //         .proxy
                //         .http_profile_route_actual
                //         .to_layer::<classify::Response, _, RouteParams>(),
                // )
                // // Depending on whether or not the request can be
                // // retried, it may have one of two `Body` types. This
                // // layer unifies any `Body` type into `BoxBody`.
                // .push_on_service(http::BoxRequest::erased())
                // // Sets an optional retry policy.
                // .push(retry::layer(
                //     rt.metrics.proxy.http_profile_route_retry.clone(),
                // ))
                // // Sets an optional request timeout.
                // .push(http::NewTimeout::layer())
                // // Records per-route metrics.
                // .push(
                //     rt.metrics
                //         .proxy
                //         .http_profile_route
                //         .to_layer::<classify::Response, _, RouteParams>(),
                // )
                // // Sets the per-route response classifier as a request
                // // extension.
                // .push(classify::NewClassify::layer())
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
                .push(svc::NewOneshotRoute::<Params, _, _>::layer_cached());

            // For each `Logical` target, watch its `Profile`, rebuilding a
            // router stack.
            concrete
                // Share the concrete stack with each router stack.
                .push_new_clone()
                // Rebuild this router stack every time the profile changes.
                .push_on_service(router)
                // Add l5d-dst-canonical header to requests.
                //
                // TODO(ver) move this into the endpoint stack so that we can only
                // set this on meshed connections.
                //
                // TODO(ver) do we need to strip headers here?
                .push_on_service(http::NewHeaderFromTarget::<CanonicalDstHeader, _>::layer())
                .push(svc::NewSpawnWatch::<ClientPolicy, _>::layer_into::<Params>())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Params ===

impl<T: svc::Param<http::Version>> From<(ClientPolicy, T)> for Params {
    fn from((policy, target): (ClientPolicy, T)) -> Self {
        Params {
            policy,
            version: target.param(),
        }
    }
}

/*
impl From<(Profile, Logical)> for Params {
    fn from((profile, logical): (Profile, Logical)) -> Self {
        // Create concrete targets for all of the profile's routes.
        let (backends, distribution) = if profile.targets.is_empty() {
            let concrete = super::Concrete {
                resolve: ConcreteAddr(logical.logical_addr.clone().into()),
                logical: logical.clone(),
            };
            let backends = std::iter::once(concrete.clone()).collect();
            let distribution = Distribution::first_available(std::iter::once(concrete));
            (backends, distribution)
        } else {
            let backends = profile
                .targets
                .iter()
                .map(|t| super::Concrete {
                    resolve: ConcreteAddr(t.addr.clone()),
                    logical: logical.clone(),
                })
                .collect();
            let distribution = Distribution::random_available(profile.targets.iter().cloned().map(
                |profiles::Target { addr, weight }| {
                    let concrete = Concrete {
                        logical: logical.clone(),
                        resolve: ConcreteAddr(addr),
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
                    profile,
                    logical: logical.clone(),
                    distribution: distribution.clone(),
                };
                (req_match, params)
            })
            // Add a default route.
            .chain(std::iter::once((
                profiles::http::RequestMatch::default(),
                RouteParams {
                    profile: Default::default(),
                    logical: logical.clone(),
                    distribution: distribution.clone(),
                },
            )))
            .collect::<Arc<[(_, _)]>>();

        Self {
            logical,
            backends,
            routes,
        }
    }
}

impl svc::Param<profiles::LogicalAddr> for Params {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical.logical_addr.clone()
    }
}
*/

impl svc::Param<CanonicalDstHeader> for Params {
    fn param(&self) -> CanonicalDstHeader {
        CanonicalDstHeader(self.policy.addr.into())
    }
}

impl svc::Param<distribute::Backends<Concrete>> for Params {
    fn param(&self) -> distribute::Backends<Concrete> {
        distribute::Backends::from_iter(self.policy.backends.iter().cloned().map(|backend| {
            Concrete {
                backend,
                version: self.version,
            }
        }))
    }
}

impl<B> svc::router::SelectRoute<http::Request<B>> for Params {
    type Key = RouteParams;
    type Error = NoRoute;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        // profiles::http::route_for_request(&*self.routes, req)
        //     .ok_or(NoRoute)
        //     .cloned()
        let policy = match self.policy.protocol {
            policy::Protocol::Http1(policy::http::Http1 { routes })
            | policy::Protocol::Http2(policy::http::Http2 { routes }) => {
                let (_, policy) = policy::http::find(&*routes, req).ok_or(NoRoute(()))?;
                policy.clone()
            }
            _ => return Err(NoRoute(())),
        };
        Ok(RouteParams {
            policy,
            version: self.version,
        })
    }
}

// === impl RouteParams ===

// impl svc::Param<profiles::LogicalAddr> for RouteParams {
//     fn param(&self) -> profiles::LogicalAddr {
//         self.logical.logical_addr.clone()
//     }
// }

impl svc::Param<Distribution> for RouteParams {
    fn param(&self) -> Distribution {
        match self.policy.distribution {
            policy::RouteDistribution::Empty => Distribution::Empty,
            policy::RouteDistribution::FirstAvailable(backends) => {
                Distribution::first_available(backends.iter().cloned().map(|rb| Concrete {
                    backend: rb.backend,
                    version: self.version,
                }))
            }
            policy::RouteDistribution::RandomAvailable(backends) => {
                Distribution::random_available(backends.iter().cloned().map(|(rb, weight)| {
                    let c = Concrete {
                        backend: rb.backend,
                        version: self.version,
                    };
                    (c, weight)
                }))
                .expect("distribution must be valid")
            }
        }
    }
}

// impl svc::Param<profiles::http::Route> for RouteParams {
//     fn param(&self) -> profiles::http::Route {
//         self.profile.clone()
//     }
// }

// impl svc::Param<metrics::ProfileRouteLabels> for RouteParams {
//     fn param(&self) -> metrics::ProfileRouteLabels {
//         metrics::ProfileRouteLabels::outbound(self.logical.logical_addr.clone(), &self.profile)
//     }
// }

// impl svc::Param<http::ResponseTimeout> for RouteParams {
//     fn param(&self) -> http::ResponseTimeout {
//         http::ResponseTimeout(self.profile.timeout())
//     }
// }

// impl classify::CanClassify for RouteParams {
//     type Classify = classify::Request;

//     fn classify(&self) -> classify::Request {
//         self.profile.response_classes().clone().into()
//     }
// }

// === impl Concrete ===

impl svc::Param<policy::Backend> for Concrete {
    fn param(&self) -> policy::Backend {
        self.backend.into()
    }
}

impl svc::Param<http::Version> for Concrete {
    fn param(&self) -> http::Version {
        self.version
    }
}
