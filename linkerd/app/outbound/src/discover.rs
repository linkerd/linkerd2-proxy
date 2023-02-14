use crate::{http, Outbound};
use linkerd_app_core::{
    profiles,
    svc::{self, stack::Param},
    transport::addrs::*,
    Error, Infallible,
};
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
};
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

    pub fn push_discover_cache<T, Req, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<Req, Response = NSvc::Response, Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
        Req: Send + 'static,
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        NSvc: svc::Service<Req, Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, stk| {
            stk.push_on_service(
                rt.metrics
                    .proxy
                    .stack
                    .layer(crate::stack_labels("tcp", "discover")),
            )
            .push(svc::NewQueue::layer_via(config.tcp_connection_queue))
            .push_new_idle_cached(config.discovery_idle_timeout)
            .push(svc::ArcNewService::layer())
            .check_new_service::<T, Req>()
        })
    }
}

// === impl Discovery ===

impl<T> From<(Option<profiles::Receiver>, T)> for Discovery<T> {
    fn from((profile, parent): (Option<profiles::Receiver>, T)) -> Self {
        Self { parent, profile }
    }
}

impl<T> svc::Param<http::Version> for Discovery<T>
where
    T: svc::Param<http::Version>,
{
    fn param(&self) -> http::Version {
        self.parent.param()
    }
}

impl<T> svc::Param<OrigDstAddr> for Discovery<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.parent.param()
    }
}

impl<T> svc::Param<Remote<ServerAddr>> for Discovery<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        self.parent.param()
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
