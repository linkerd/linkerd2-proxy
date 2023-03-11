use crate::{policy, Outbound};
use linkerd_app_core::{profiles, svc, transport::OrigDstAddr, Error};
use std::{
    fmt::Debug,
    hash::{Hash, Hasher},
    net::SocketAddr,
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
    queue: policy::Queue,
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
    svc::mk(move |OrigDstAddr(orig_dst)| {
        let profile = profiles
            .clone()
            .get_profile(profiles::LookupAddr(orig_dst.into()));
        let policy = policies.get_policy(orig_dst.into());
        Box::pin(async move {
            let (profile, policy) = tokio::join!(profile, policy);

            let profile = profile.unwrap_or_else(|error| {
                tracing::warn!(%error, "Error resolving ServiceProfile");
                None
            });

            // If there was a policy resolution, return it with the profile so
            // the stack can determine how to switch on them.
            match policy {
                Ok(policy) => return Ok((profile, policy)),
                Err(error) => {
                    if error
                        .downcast_ref::<tonic::Status>()
                        .map(|s| s.code() != tonic::Code::NotFound)
                        .unwrap_or(true)
                    {
                        return Err(error);
                    }
                }
            }

            // XXX(ver) The policy controller may (for the time being) reject
            // our lookups, since it doesn't yet serve endpoint metadata for
            // forwarding.
            tracing::debug!("Policy not found");

            // If there was a profile resolution, try to use it to synthesize a
            // enpdoint policy.
            if let Some(profile) = profile {
                let policy = spawn_synthesized_profile_policy(
                    orig_dst,
                    profile.clone().into(),
                    queue,
                    detect_timeout,
                );
                return Ok((Some(profile), policy));
            }

            // Otherwise, route the request to the original destination address.
            let policy = spawn_synthesized_origdst_policy(orig_dst, queue, detect_timeout);
            Ok((None, policy))
        })
    })
}

fn spawn_synthesized_profile_policy(
    orig_dst: SocketAddr,
    mut profile: watch::Receiver<profiles::Profile>,
    queue: policy::Queue,
    detect_timeout: Duration,
) -> watch::Receiver<policy::ClientPolicy> {
    let mk_policy = move |profile: &profiles::Profile| {
        let (addr, meta) = profile
            .endpoint
            .clone()
            .unwrap_or_else(|| (orig_dst, Default::default()));
        let forward = policy::BackendDispatcher::Forward(addr, meta);
        policy::ClientPolicy::from_backend(detect_timeout, queue, forward)
    };

    let policy = mk_policy(&*profile.borrow_and_update());
    tracing::debug!(?policy, "Synthesizing policy from profile");
    let (tx, rx) = watch::channel(policy);
    tokio::spawn(async move {
        let mut profile = profile;
        loop {
            if profile.changed().await.is_err() {
                tracing::debug!("Profile watch closed, terminating");
                return;
            };
            let policy = mk_policy(&*profile.borrow());
            tracing::debug!(?policy, "Profile updated; synthesizing policy");
            if tx.send(policy).is_err() {
                tracing::debug!("Policy watch closed, terminating");
                return;
            }
        }
    });
    rx
}

fn spawn_synthesized_origdst_policy(
    orig_dst: SocketAddr,
    queue: policy::Queue,
    detect_timeout: Duration,
) -> watch::Receiver<policy::ClientPolicy> {
    let forward = policy::BackendDispatcher::Forward(orig_dst, Default::default());
    let policy = policy::ClientPolicy::from_backend(detect_timeout, queue, forward);
    tracing::debug!(?policy, "Synthesizing policy");
    let (tx, rx) = watch::channel(policy);
    tokio::spawn(async move {
        tx.closed().await;
    });
    rx
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
