use super::{AllowPolicy, LookupAddr};
use futures::future;
use linkerd_app_core::{svc, transport::ServerAddr, Error};
use linkerd_idle_cache::IdleCache;
pub use linkerd_proxy_server_policy::{
    authz::Suffix, Authentication, Authorization, Protocol, ServerPolicy,
};
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, Hasher},
    task,
};
use tokio::{sync::watch, time::Duration};
use tower::ServiceExt;
use tracing::{info_span, Instrument};

#[derive(Clone)]
pub struct Store<D> {
    cache: IdleCache<u16, Rx, BuildHasherDefault<PortHasher>>,
    discover: D,
}

type Rx = watch::Receiver<ServerPolicy>;

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

// === impl Store ===

impl<D> Store<D>
where
    D: svc::Service<
        u16,
        Response = tonic::Response<watch::Receiver<ServerPolicy>>,
        Error = tonic::Status,
    >,
    D: Clone + Send + Sync + 'static,
    D::Future: Send,
{
    #[cfg(test)]
    pub(super) fn spawn_fixed(
        idle_timeout: Duration,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
        discover: D,
    ) -> Self {
        let cache = {
            let rxs = ports.into_iter().map(|(p, s)| {
                // When using a fixed policy, we don't need to watch for changes. It's
                // safe to discard the sender, as the receiver will continue to let us
                // borrow/clone each fixed policy.
                let (_, rx) = watch::channel(s);
                (p, rx)
            });
            IdleCache::with_permanent_from_iter(idle_timeout, rxs)
        };

        Self { cache, discover }
    }

    /// Spawns a watch for each of the given ports.
    ///
    /// A discovery watch is spawned for each of the described `ports` and the
    /// result is cached for as long as the `Store` is held. The `Store` may be used to
    pub(super) fn spawn_discover(
        idle_timeout: Duration,
        discover: D,
        ports: &HashSet<u16>,
    ) -> Self {
        // The initial set of policies never expire from the cache.
        //
        // Policies that are dynamically discovered at runtime will expire after
        // `idle_timeout` to prevent holding policy watches indefinitely for
        // ports that are generally unused.
        let cache = IdleCache::with_capacity(idle_timeout, ports.len());
        for &port in ports.into_iter() {
            let cache = cache.clone();
            let discover = discover.clone();
            tokio::spawn(
                async move {
                    match discover.oneshot(port).await {
                        Err(error) => {
                            // we'll try again when a connection actually targets this port...
                            tracing::error!(port, %error, "Failed to start ServerPolicy default port watch!")
                        }
                        Ok(rsp) => {
                            let rx = rsp.into_inner();
                            cache.insert_permanent(port, rx);
                        }
                    }
                }
                .instrument(info_span!("watch", port)),
            );
        }

        Self { cache, discover }
    }
}

impl<D> svc::Service<LookupAddr> for Store<D>
where
    D: svc::Service<
        u16,
        Response = tonic::Response<watch::Receiver<ServerPolicy>>,
        Error = tonic::Status,
    >,
    D: Clone + Send + Sync + 'static,
    D::Future: Send,
{
    type Response = AllowPolicy;
    type Error = Error;
    type Future = future::Either<
        future::Ready<Result<AllowPolicy, Error>>,
        future::BoxFuture<'static, Result<AllowPolicy, Error>>,
    >;

    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, LookupAddr(addr): LookupAddr) -> Self::Future {
        // Lookup the policy for the target port in the cache. If it doesn't
        // already exist, we spawn a watch on the API (if it is configured). If
        // no discovery API is configured we use the default policy.
        let port = addr.port();
        let dst = ServerAddr(addr);
        if let Some(server) = self.cache.get(&port) {
            return future::Either::Left(future::ready(Ok(AllowPolicy { dst, server })));
        }

        let disco = self.discover.clone();
        let cache = self.cache.clone();
        future::Either::Right(Box::pin(
            async move {
                tracing::trace!(%port, "spawning policy discovery");
                let server = disco.oneshot(port).await?.into_inner();
                let server = cache.get_or_insert(port, server);
                Ok(AllowPolicy { dst, server })
            }
            .instrument(info_span!("watch", port)),
        ))
    }
}

// === impl PortHasher ===

impl Hasher for PortHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("hashing a `u16` calls `write_u16`");
    }

    #[inline]
    fn write_u16(&mut self, port: u16) {
        self.0 = port;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 as u64
    }
}
