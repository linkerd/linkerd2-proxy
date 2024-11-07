use crate::{policy, service_meta, tcp, Outbound, ParentRef, UNKNOWN_META};
use linkerd_app_core::{
    io,
    metrics::prom,
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Addr, Error,
};
use once_cell::sync::Lazy;
use std::{fmt::Debug, hash::Hash, sync::Arc};
use tokio::sync::watch;

mod concrete;
mod logical;

pub use self::logical::{Concrete, Logical, Routes};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq<T>(T);

#[derive(Clone, Debug, Default)]
pub struct OpaqMetrics {
    balance: concrete::BalancerMetrics,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that proxies opaque TCP connections.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_opaq_cached<T, I, R>(self, resolve: R) -> Outbound<svc::ArcNewCloneTcp<T, I>>
    where
        // Opaque target
        T: Clone + Debug + PartialEq + Eq + Hash + Send + Sync + 'static,
        T: svc::Param<watch::Receiver<Routes>>,
        // Server-side connection
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Endpoint discovery
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Unpin,
        // TCP endpoint stack.
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
    {
        self.push_tcp_endpoint()
            .push_opaq_concrete(resolve)
            .push_opaq_logical()
            .map_stack(|config, _rt, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    // Use a dedicated target type to configure parameters for
                    // the opaque stack. It also helps narrow the cache key.
                    .push_map_target(Opaq)
                    .arc_new_clone_tcp()
            })
    }
}

// === impl Opaq ===

impl<T> svc::Param<watch::Receiver<logical::Routes>> for Opaq<T>
where
    T: svc::Param<watch::Receiver<logical::Routes>>,
{
    fn param(&self) -> watch::Receiver<logical::Routes> {
        self.0.param()
    }
}

// === impl OpaqMetrics ===

impl OpaqMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let balance =
            concrete::BalancerMetrics::register(registry.sub_registry_with_prefix("balancer"));
        Self { balance }
    }
}

fn should_override_opaq_policy(
    rx: &watch::Receiver<profiles::Profile>,
) -> Option<profiles::LogicalAddr> {
    let p = rx.borrow();
    if p.has_targets() {
        p.addr.clone()
    } else {
        None
    }
}
/// Given both profiles and policy information, this function constructs `opaq::Routes``.
/// The decision on whether profiles or policy should be used is made by inspecting the
/// returned profiles and checking whether there are any targets defined. This is done
/// in order to support traffic splits. Everything else should be delivered through client
/// policy.
pub fn routes_from_discovery(
    addr: Addr,
    profile: Option<profiles::Receiver>,
    mut policy: policy::Receiver,
) -> watch::Receiver<Routes> {
    if let Some(mut profile) = profile.map(watch::Receiver::from) {
        if let Some(paddr) = should_override_opaq_policy(&profile) {
            tracing::debug!("Using ServiceProfile");
            let init = routes_from_profile(paddr.clone(), &profile.borrow_and_update());

            return spawn_routes(profile, init, move |profile: &profiles::Profile| {
                Some(routes_from_profile(paddr.clone(), profile))
            });
        }
    }

    tracing::debug!("Using ClientPolicy routes");
    let init = routes_from_policy(addr.clone(), &policy.borrow_and_update())
        .expect("initial policy must be opaque");

    spawn_routes(policy, init, move |policy: &policy::ClientPolicy| {
        routes_from_policy(addr.clone(), policy)
    })
}

fn routes_from_policy(addr: Addr, policy: &policy::ClientPolicy) -> Option<Routes> {
    let routes = match policy.protocol {
        policy::Protocol::Opaque(policy::opaq::Opaque { ref routes }) => routes.clone(),
        // we support a detect stack to cover the case when we do detection and fallback to opaq
        policy::Protocol::Detect { ref opaque, .. } => opaque.routes.clone(),
        _ => {
            tracing::info!("Ignoring a discovery update that changed a route from opaq");
            return None;
        }
    };

    Some(Routes {
        logical: Logical {
            addr,
            meta: ParentRef(policy.parent.clone()),
        },
        routes,
        backends: policy.backends.clone(),
    })
}

fn routes_from_profile(
    profiles::LogicalAddr(profiles_addr): profiles::LogicalAddr,
    profile: &profiles::Profile,
) -> Routes {
    // TODO: make configurable
    let queue = {
        policy::Queue {
            capacity: 100,
            failfast_timeout: std::time::Duration::from_secs(3),
        }
    };

    const EWMA: policy::Load = policy::Load::PeakEwma(policy::PeakEwma {
        default_rtt: std::time::Duration::from_millis(30),
        decay: std::time::Duration::from_secs(10),
    });

    // TODO(ver) use resource metadata from the profile response.
    let parent_meta = service_meta(&profiles_addr).unwrap_or_else(|| UNKNOWN_META.clone());

    let backends: Vec<(policy::RouteBackend<policy::opaq::Filter>, u32)> = profile
        .targets
        .iter()
        .map(|target| {
            // TODO(ver) use resource metadata from the profile response.
            let backend_meta = service_meta(&target.addr).unwrap_or_else(|| UNKNOWN_META.clone());
            let backend = policy::RouteBackend {
                backend: policy::Backend {
                    meta: backend_meta,
                    queue,
                    dispatcher: policy::BackendDispatcher::BalanceP2c(
                        EWMA,
                        policy::EndpointDiscovery::DestinationGet {
                            path: target.addr.to_string(),
                        },
                    ),
                },
                filters: std::sync::Arc::new([]),
            };

            (backend, target.weight)
        })
        .collect();

    let distribution = policy::RouteDistribution::RandomAvailable(backends.clone().into());

    static ROUTE_META: Lazy<Arc<policy::Meta>> =
        Lazy::new(|| policy::Meta::new_default("serviceprofile"));
    let route = policy::opaq::Route {
        policy: policy::opaq::Policy {
            // TODO(ver) use resource metadata from the profile response.
            meta: ROUTE_META.clone(),
            params: (),
            filters: std::sync::Arc::new([]),
            distribution,
        },
    };

    Routes {
        logical: Logical {
            addr: profiles_addr.into(),
            meta: ParentRef(parent_meta),
        },
        backends: backends.into_iter().map(|(b, _)| b.backend).collect(),
        routes: Some(route),
    }
}

fn spawn_routes<T>(
    mut route_rx: watch::Receiver<T>,
    init: Routes,
    mut mk: impl FnMut(&T) -> Option<Routes> + Send + Sync + 'static,
) -> watch::Receiver<Routes>
where
    T: Send + Sync + 'static,
{
    let (tx, rx) = watch::channel(init);

    tokio::spawn(async move {
        loop {
            let res = tokio::select! {
                biased;
                _ = tx.closed() => return,
                res = route_rx.changed() => res,
            };

            if res.is_err() {
                // Drop the `tx` sender when the profile sender is
                // dropped.
                return;
            }

            if let Some(routes) = (mk)(&*route_rx.borrow_and_update()) {
                if tx.send(routes).is_err() {
                    // Drop the `tx` sender when all of its receivers are dropped.
                    return;
                }
            }
        }
    });

    rx
}
