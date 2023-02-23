use super::{api, AllowPolicy, DefaultPolicy, LookupAddr};
use futures::future;
use linkerd_app_core::{proxy::http, svc, transport::ServerAddr, Error};
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
use tracing::{info_span, Instrument};

#[derive(Clone)]
pub struct Store<S> {
    cache: IdleCache<u16, Rx, BuildHasherDefault<PortHasher>>,
    default_rx: Rx,
    discover: Option<api::Watch<S>>,
}

type Rx = watch::Receiver<ServerPolicy>;

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

// === impl Store ===

impl<S> Store<S> {
    pub(crate) fn spawn_fixed(
        default: DefaultPolicy,
        idle_timeout: Duration,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
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

        Self {
            cache,
            discover: None,
            default_rx: Self::spawn_default(default),
        }
    }

    /// Spawns a watch for each of the given ports.
    ///
    /// A discovery watch is spawned for each of the described `ports` and the
    /// result is cached for as long as the `Store` is held. The `Store` may be used to
    pub(super) fn spawn_discover(
        default: DefaultPolicy,
        idle_timeout: Duration,
        discover: api::Watch<S>,
        ports: HashSet<u16>,
    ) -> Self
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody:
            http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    {
        // The initial set of policies never expire from the cache.
        //
        // Policies that are dynamically discovered at runtime will expire after
        // `idle_timeout` to prevent holding policy watches indefinitely for
        // ports that are generally unused.
        let cache = {
            let rxs = ports.into_iter().map(|port| {
                let discover = discover.clone();
                let default = default.clone();
                let rx = info_span!("watch", port)
                    .in_scope(|| discover.spawn_with_init(port, default.into()));
                (port, rx)
            });
            IdleCache::with_permanent_from_iter(idle_timeout, rxs)
        };

        Self {
            cache,
            discover: Some(discover),
            default_rx: Self::spawn_default(default),
        }
    }

    fn spawn_default(default: DefaultPolicy) -> Rx {
        let (tx, rx) = watch::channel(ServerPolicy::from(default));
        // Hold the sender until all of the receivers are dropped. This ensures
        // that receivers can be watched like any other policy.
        tokio::spawn(async move {
            tx.closed().await;
        });
        rx
    }
}

impl<S> svc::Service<LookupAddr> for Store<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
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

        match self.discover {
            None => {
                // If no discovery API is configured, then we use the
                // default policy. While it's a little wasteful to cache
                // these results separately, this case isn't expected to be
                // used outside of testing.
                tracing::trace!(%port, "using the default policy");
                let server = self.cache.get_or_insert(port, self.default_rx.clone());
                future::Either::Left(future::ready(Ok(AllowPolicy { dst, server })))
            }
            Some(ref disco) => {
                let disco = disco.clone();
                let cache = self.cache.clone();
                future::Either::Right(Box::pin(
                    async move {
                        tracing::trace!(%port, "spawning policy discovery");
                        let server = disco.spawn_watch(port).await?.into_inner();
                        let server = cache.get_or_insert(port, server);
                        Ok(AllowPolicy { dst, server })
                    }
                    .instrument(info_span!("watch", port)),
                ))
            }
        }
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
