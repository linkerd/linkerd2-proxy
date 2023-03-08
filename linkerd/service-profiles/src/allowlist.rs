use crate::{GetProfile, LookupAddr, Receiver};
use futures::future;
use linkerd_addr::AddrMatch;

/// Wraps a `GetProfile` implementation with an allowlist of addresses.
#[derive(Clone, Debug)]
pub struct WithAllowlist<G> {
    allow_discovery: AddrMatch,
    inner: G,
}

// === impl WithAllowlist ===

impl<G> WithAllowlist<G> {
    pub fn new(inner: G, allow_discovery: AddrMatch) -> Self {
        Self {
            inner,
            allow_discovery,
        }
    }
}

impl<G> GetProfile for WithAllowlist<G>
where
    G: GetProfile,
    G::Error: Send,
{
    type Future = future::Either<future::Ready<Result<Option<Receiver>, Self::Error>>, G::Future>;
    type Error = G::Error;

    fn get_profile(&mut self, addr: LookupAddr) -> Self::Future {
        if self.allow_discovery.matches(&addr.0) {
            tracing::debug!(%addr, "Discovery allowed");
            return future::Either::Right(self.inner.get_profile(addr));
        }
        tracing::debug!(
            %addr,
            domains = %self.allow_discovery.names(),
            networks = %self.allow_discovery.nets(),
            "Discovery skipped",
        );
        future::Either::Left(future::ok(None))
    }
}
