use crate::{policy, Outbound};
use linkerd_app_core::{profiles, svc, transport::OrigDstAddr, Error};
use std::{
    fmt::Debug,
    hash::{Hash, Hasher},
    ops::Deref,
    time::Duration,
};
use tokio::sync::watch;

#[cfg(test)]
mod tests;

/// Target with a discovery result.
#[derive(Clone, Debug)]
pub struct Discovery<T> {
    parent: T,
    profile: Option<profiles::Receiver>,
    policy: Option<policy::Receiver>,
}

impl<N> Outbound<N> {
    /// Discovers routing configuration.
    pub fn push_discover<T, K, Req, NSvc, D>(
        self,
        discover: D,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: svc::Param<K>,
        T: Clone + Send + Sync + 'static,
        K: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        // Request type.
        Req: Send + 'static,
        // Discovery client.
        D: svc::Service<K, Error = Error> + Clone + Send + Sync + 'static,
        D::Future: Send + Unpin + 'static,
        D::Error: Send + Sync + 'static,
        D::Response: Clone + Send + Sync + 'static,
        Discovery<T>: From<(D::Response, T)>,
        // Inner stack.
        N: svc::NewService<Discovery<T>, Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        NSvc: svc::Service<Req, Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, _, stk| {
            stk.lift_new_with_target()
                .push_new_cached_discover(discover, config.discovery_idle_timeout)
                .check_new_service::<T, _>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

pub(crate) fn resolver(
    profiles: impl profiles::GetProfile<Error = Error>,
    policies: impl policy::GetPolicy,
    default_queue: policy::Queue,
    detect_timeout: Duration,
) -> impl svc::Service<
    OrigDstAddr,
    Error = Error,
    Response = (Option<profiles::Receiver>, policy::Receiver),
    Future = impl Send,
> + Clone
       + Send
       + Sync
       + 'static {
    svc::mk(move |addr: OrigDstAddr| {
        let profile = profiles
            .clone()
            .get_profile(profiles::LookupAddr(addr.0.into()));
        let policy = policies.get_policy(addr.0.into());
        Box::pin(async move {
            let (profile, policy) = tokio::join!(profile, policy);
            let profile = profile.unwrap_or_else(|error| {
                tracing::warn!(%error, "Error resolving ServiceProfile");
                None
            });

            let policy_error = match policy {
                Ok(policy) => return Ok((profile, policy)),
                Err(error) => error,
            };

            // Was the policy resolution error a gRPC `NotFound` response?
            let policy_not_found = policy_error
                .downcast_ref::<tonic::Status>()
                .map(|status| status.code() == tonic::Code::NotFound)
                .unwrap_or(false);

            // If the policy controller responded with `NotFound`, synthesize a
            // policy, either from the ServiceProfile if one was resolved, or
            // for a forward to the original destination address.
            if policy_not_found {
                if let Some(profile) = profile {
                    let mut profile_rx = watch::Receiver::from(profile.clone());
                    let ewma = policy::Load::PeakEwma(policy::PeakEwma {
                        default_rtt: crate::http::logical::profile::DEFAULT_EWMA.default_rtt,
                        decay: crate::http::logical::profile::DEFAULT_EWMA.decay,
                    });
                    let policy_from_profile = move |profile: &profiles::Profile| {
                        let dispatcher = if let Some((addr, meta)) = profile.endpoint.clone() {
                            policy::BackendDispatcher::Forward(addr, meta)
                        } else if let Some(profiles::LogicalAddr(ref addr)) = profile.addr {
                            policy::BackendDispatcher::BalanceP2c(
                                ewma,
                                policy::EndpointDiscovery::DestinationGet {
                                    path: addr.to_string(),
                                },
                            )
                        } else {
                            policy::BackendDispatcher::Forward(addr.0, Default::default())
                        };
                        policy::ClientPolicy::from_backend(
                            detect_timeout,
                            default_queue,
                            dispatcher,
                        )
                    };

                    let policy = policy_from_profile(&*profile_rx.borrow_and_update());
                    tracing::debug!(
                        ?policy,
                        "Policy not found; synthesizing policy from profile..."
                    );
                    let (tx, policy_rx) = watch::channel(policy);
                    tokio::spawn(async move {
                        loop {
                            if profile_rx.changed().await.is_err() {
                                tracing::debug!("Profile watch closed, terminating");
                                return;
                            }

                            let policy = policy_from_profile(&*profile_rx.borrow_and_update());

                            tracing::debug!(
                                ?policy,
                                "Profile updated; synthesizing policy from profile..."
                            );
                            if tx.send(policy).is_err() {
                                tracing::debug!("Policy watch closed, terminating");
                                return;
                            }
                        }
                    });
                    return Ok((Some(profile), policy_rx));
                }

                let policy = policy::ClientPolicy::from_backend(
                    detect_timeout,
                    default_queue,
                    policy::BackendDispatcher::Forward(addr.0, Default::default()),
                );
                tracing::debug!(
                    ?policy,
                    "Policy not found; no profile: synthesizing policy from original destination address"
                );

                let (tx, policy_rx) = watch::channel(policy);
                // keep the watch channel alive as long as it's in use.
                tokio::spawn(async move { tx.closed().await });
                return Ok((None, policy_rx));
            }

            // Otherwise, errors resolving the policy are considered fatal.
            Err(policy_error)
        })
    })
}

// === impl Discovery ===

impl<T> From<((Option<profiles::Receiver>, policy::Receiver), T)> for Discovery<T> {
    fn from(
        // whew!
        ((profile, policy), parent): ((Option<profiles::Receiver>, policy::Receiver), T),
    ) -> Self {
        Self {
            parent,
            profile,
            policy: Some(policy),
        }
    }
}

impl<T> From<(Option<profiles::Receiver>, T)> for Discovery<T> {
    fn from((profile, parent): (Option<profiles::Receiver>, T)) -> Self {
        Self {
            parent,
            profile,
            policy: None,
        }
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Discovery<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl<T> svc::Param<Option<watch::Receiver<profiles::Profile>>> for Discovery<T> {
    fn param(&self) -> Option<watch::Receiver<profiles::Profile>> {
        self.profile.clone().map(Into::into)
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Discovery<T> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.as_ref().and_then(|p| p.logical_addr())
    }
}

impl<T> svc::Param<Option<policy::Receiver>> for Discovery<T> {
    fn param(&self) -> Option<policy::Receiver> {
        self.policy.clone()
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
