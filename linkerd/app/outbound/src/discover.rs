use crate::{policy, Outbound};
use linkerd_app_core::{
    profiles,
    svc::{self, stack::Param},
    Error,
};
pub use policy::TargetAddr;
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
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
    pub fn push_discover<T, Req, NSvc, D>(
        self,
        discover: D,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: Param<TargetAddr>,
        T: Clone + Send + Sync + 'static,
        // Request type.
        Req: Send + 'static,
        // Discovery client.
        D: svc::Service<TargetAddr, Error = Error> + Clone + Send + Sync + 'static,
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
