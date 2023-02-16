#![allow(unused_imports)]

use crate::Outbound;
use futures::prelude::*;
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
        let detect_timeout = self.config.proxy.detect_protocol_timeout;
        let discover_policy = svc::mk(move |addr: Addr| {
            tracing::debug!("Looking up policy");
            let rx = crate::spawn_default_client_policy_rx(addr, queue, detect_timeout);
            futures::future::ok::<_, Infallible>(rx)
        });

        let allow = self.config.allow_discovery.clone();
        let discover_profile = svc::mk(move |addr: Addr| {
            let allow = allow.clone();
            let profiles = profiles.clone();
            Box::pin(async move {
                if allow.matches(&addr) {
                    tracing::debug!("Looking up profile");
                    profiles
                        .get_profile(profiles::LookupAddr(addr.clone()))
                        .await
                } else {
                    debug!(
                        %addr,
                        domains = %allow.names(),
                        networks = %allow.nets(),
                        "Discovery skipped",
                    );
                    Ok(None)
                }
            })
        });

        let discover = svc::mk(move |profiles::LookupAddr(addr): profiles::LookupAddr| {
            tracing::debug!("Discovering");
            let discover_profile = discover_profile.clone();
            Box::pin(async move {
                tokio::try_join!(
                    discover_profile.oneshot(addr.clone()),
                    discover_policy.oneshot(addr).map_err(Into::into)
                )
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
        // Option<profiles::Receiver>,
        T,
    )> for Discovery<T>
{
    fn from(
        ((profile, policy), parent): (
            // (profile, parent): (
            (Option<profiles::Receiver>, watch::Receiver<ClientPolicy>),
            // Option<profiles::Receiver>,
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
