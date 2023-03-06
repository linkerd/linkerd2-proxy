use crate::Outbound;
use futures::future;
use linkerd_app_core::{
    profiles,
    svc::{self, stack::Param},
    Addr, AddrMatch, Error,
};
use std::{
    hash::{Hash, Hasher},
    ops::Deref,
    task::{Context, Poll},
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

/// Wraps a discovery service with an allowlist of addresses.
#[derive(Clone, Debug)]
pub struct WithAllowlist<S> {
    allow_discovery: AddrMatch,
    inner: S,
}

impl<N> Outbound<N> {
    /// Discovers routing configuration.
    pub fn push_discover<T, Req, NSvc, D>(
        self,
        discover: D,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + 'static,
        // Request type.
        Req: Send + 'static,
        // Discovery client.
        D: svc::Service<profiles::LookupAddr, Error = Error> + Clone + Send + Sync + 'static,
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
        self.profile.clone().map(Into::into)
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

// === impl WithAllowlist ===

impl<S> WithAllowlist<S> {
    pub fn new(inner: S, allow_discovery: AddrMatch) -> Self {
        Self {
            inner,
            allow_discovery,
        }
    }
}

impl<S, D, T> svc::Service<T> for WithAllowlist<S>
where
    S: svc::Service<T, Response = Option<D>>,
    T: AsRef<Addr>,
{
    type Response = Option<D>;
    type Future = futures::future::Either<
        futures::future::Ready<Result<Self::Response, Self::Error>>,
        S::Future,
    >;
    type Error = S::Error;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let addr = target.as_ref();
        // TODO(ver) Should this allowance be parameterized by
        // the target type?
        if self.allow_discovery.matches(addr) {
            debug!(%addr, "Discovery allowed");
            return future::Either::Right(self.inner.call(target));
        }
        debug!(
            %addr,
            domains = %self.allow_discovery.names(),
            networks = %self.allow_discovery.nets(),
            "Discovery skipped",
        );
        future::Either::Left(future::ok(None))
    }
}
