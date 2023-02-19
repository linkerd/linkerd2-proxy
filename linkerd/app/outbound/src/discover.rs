use crate::Outbound;
use linkerd_app_core::{
    profiles,
    svc::{self, stack::Param},
    Error, Infallible,
};
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
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
}

impl<N> Outbound<N> {
    /// Discovers routing configuration.
    pub fn push_discover<T, Req, NSvc, P>(
        self,
        profiles: P,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: Param<profiles::LookupAddr>,
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
        self.map_stack(|config, _, stk| {
            let allow = config.allow_discovery.clone();
            stk.clone()
                .lift_new_with_target()
                .push_new_cached_discover(profiles.into_service(), config.discovery_idle_timeout)
                .push_switch(
                    move |parent: T| -> Result<_, Infallible> {
                        // TODO(ver) Should this allowance be parameterized by
                        // the target type?
                        let profiles::LookupAddr(addr) = parent.param();
                        if allow.matches(&addr) {
                            debug!(%addr, "Discovery allowed");
                            return Ok(svc::Either::A(parent));
                        }
                        debug!(
                            %addr,
                            domains = %allow.names(),
                            networks = %allow.nets(),
                            "Discovery skipped",
                        );
                        Ok(svc::Either::B(Discovery {
                            parent,
                            profile: None,
                        }))
                    },
                    stk.into_inner(),
                )
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Discovery ===

impl<T> From<(Option<profiles::Receiver>, T)> for Discovery<T> {
    fn from((profile, parent): (Option<profiles::Receiver>, T)) -> Self {
        Self { parent, profile }
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Discovery<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl<T> svc::Param<Option<watch::Receiver<profiles::Profile>>> for Discovery<T> {
    fn param(&self) -> Option<watch::Receiver<profiles::Profile>> {
        let p = self.profile.clone()?;
        Some(p.into())
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
