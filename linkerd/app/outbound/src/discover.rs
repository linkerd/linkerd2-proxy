use crate::Outbound;
use linkerd_app_core::{
    profiles,
    svc::{self, ServiceExt},
    Addr, Error, Infallible,
};
use linkerd_proxy_client_policy::{self as policy, ClientPolicy};
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
    time,
};
use tokio::sync::watch;
use tracing::debug;

#[cfg(test)]
mod tests;

/// Target with a discovery result.
#[derive(Clone, Debug)]
pub struct Discovery<T> {
    parent: T,
    profile: Option<profiles::Receiver>,
    policy: watch::Receiver<ClientPolicy>,
}

impl<N> Outbound<N> {
    /// Discovers routing configuration.
    pub fn push_discover<T, Req, NSvc, P>(
        self,
        profiles: P,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: svc::Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + 'static,
        // Request type.
        Req: Send + 'static,
        // Discovery client.
        P: profiles::GetProfile<Error = Error>,
        // Inner stack.
        N: svc::NewService<Discovery<T>, Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        NSvc: svc::Service<Req, Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        // Mock out the policy discovery service.
        let queue = self.config.tcp_connection_queue;
        let policy = svc::mk(move |addr: Addr| {
            Box::pin(async move {
                let (tx, rx) = watch::channel(match addr {
                    Addr::Socket(addr) => {
                        let backend = policy::Backend {
                            meta: policy::Meta::new_default("default"),
                            queue: policy::Queue {
                                capacity: queue.capacity,
                                failfast_timeout: queue.failfast_timeout,
                            },
                            dispatcher: policy::BackendDispatcher::Forward(
                                addr,
                                Default::default(),
                            ),
                        };
                        ClientPolicy {
                            protocol: policy::Protocol::Opaque(policy::opaq::Opaque {
                                policy: Some(policy::opaq::Policy {
                                    meta: policy::Meta::new_default("default"),
                                    filters: Arc::new([]),
                                    distribution: policy::RouteDistribution::FirstAvailable(
                                        Arc::new([policy::RouteBackend {
                                            filters: Arc::new([]),
                                            backend: backend.clone(),
                                        }]),
                                    ),
                                }),
                            }),
                            backends: Arc::new([backend]),
                        }
                    }

                    Addr::Name(addr) => {
                        const EWMA: policy::PeakEwma = policy::PeakEwma {
                            default_rtt: time::Duration::from_millis(30),
                            decay: time::Duration::from_secs(10),
                        };
                        let backend = policy::Backend {
                            meta: policy::Meta::new_default("default"),
                            queue: policy::Queue {
                                capacity: queue.capacity,
                                failfast_timeout: queue.failfast_timeout,
                            },
                            dispatcher: policy::BackendDispatcher::BalanceP2c(
                                policy::Load::PeakEwma(EWMA),
                                policy::EndpointDiscovery::DestinationGet {
                                    path: addr.to_string(),
                                },
                            ),
                        };
                        ClientPolicy {
                            protocol: policy::Protocol::Opaque(policy::opaq::Opaque {
                                policy: Some(policy::opaq::Policy {
                                    meta: policy::Meta::new_default("default"),
                                    filters: Arc::new([]),
                                    distribution: policy::RouteDistribution::FirstAvailable(
                                        Arc::new([policy::RouteBackend {
                                            filters: Arc::new([]),
                                            backend: backend.clone(),
                                        }]),
                                    ),
                                }),
                            }),
                            backends: Arc::new([backend]),
                        }
                    }
                });
                tokio::spawn(async move {
                    tx.closed().await;
                });
                Ok::<_, Infallible>(rx)
            })
        });

        let allow = self.config.allow_discovery.clone();
        let discover = svc::mk(move |profiles::LookupAddr(addr): profiles::LookupAddr| {
            let allow = allow.clone();
            let profiles = profiles.clone();
            Box::pin(async move {
                let profile = if !allow.matches(&addr) {
                    profiles
                        .get_profile(profiles::LookupAddr(addr.clone()))
                        .await?
                } else {
                    debug!(
                        %addr,
                        domains = %allow.names(),
                        networks = %allow.nets(),
                        "Discovery skipped",
                    );
                    None
                };

                let policy_rx = policy.oneshot(addr).await?;
                Ok((profile, policy_rx))
            })
        });

        self.map_stack(|config, _, stk| {
            stk.lift_new_with_target()
                .push_new_cached_discover(discover, config.discovery_idle_timeout)
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Discovery ===

impl<T>
    From<(
        (Option<profiles::Receiver>, watch::Receiver<ClientPolicy>),
        T,
    )> for Discovery<T>
{
    fn from(
        ((profile, policy), parent): (
            (Option<profiles::Receiver>, watch::Receiver<ClientPolicy>),
            T,
        ),
    ) -> Self {
        Self {
            parent,
            profile,
            policy,
        }
    }
}

impl<T> svc::Param<watch::Receiver<ClientPolicy>> for Discovery<T> {
    fn param(&self) -> watch::Receiver<ClientPolicy> {
        self.policy.clone()
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Discovery<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Discovery<T> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.as_ref().and_then(|p| p.logical_addr())
    }
}

impl<T> Deref for Discovery<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

impl<T: PartialEq> PartialEq for Discovery<T> {
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent
    }
}

impl<T: Eq> Eq for Discovery<T> {}

impl<T: Hash> Hash for Discovery<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.hash(state);
    }
}
