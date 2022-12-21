use super::{api, config, AllowPolicy, DefaultPolicy, GetPolicy};
use linkerd_app_core::{proxy::http, transport::OrigDstAddr, Error};
use linkerd_cache::{Cache, Cached};
pub use linkerd_server_policy::{
    authz::Suffix, Authentication, Authorization, Protocol, ServerPolicy,
};
use rangemap::RangeInclusiveSet;
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use tokio::{sync::watch, time::Duration};
use tracing::info_span;

#[derive(Clone)]
pub struct Store<S> {
    cache: Cache<u16, Rx, BuildHasherDefault<PortHasher>>,
    default_rx: Rx,
    discover: Option<api::Watch<S>>,
    opaque_ports: Option<(Arc<RangeInclusiveSet<u16>>, Rx)>,
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
    #[cfg(test)]
    pub(crate) fn spawn_fixed(
        default: DefaultPolicy,
        idle_timeout: Duration,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
        opaque_ports: Option<config::OpaquePorts>,
    ) -> Self {
        let cache = {
            let rxs = ports.into_iter().map(|(port, policy)| {
                let (_, rx) = watch::channel(policy);
                (port, rx)
            });
            Cache::with_permanent_from_iter(idle_timeout, rxs)
        };
        Self::new(cache, default, None, opaque_ports)
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
        opaque_ports: Option<config::OpaquePorts>,
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
            Cache::with_permanent_from_iter(idle_timeout, rxs)
        };
        Self::new(cache, default, Some(discover), opaque_ports)
    }

    fn new(
        cache: Cache<u16, Rx, BuildHasherDefault<PortHasher>>,
        default: DefaultPolicy,
        discover: Option<api::Watch<S>>,
        opaque_ports: Option<config::OpaquePorts>,
    ) -> Self {
        let opaque_ports = opaque_ports.map(|config::OpaquePorts { ranges, policy }| {
            (
                Arc::new(ranges),
                Self::spawn_default(DefaultPolicy::Allow(policy)),
            )
        });
        Self {
            default_rx: Self::spawn_default(default),
            opaque_ports,
            cache,
            discover,
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

impl<S> GetPolicy for Store<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    fn get_policy(&self, dst: OrigDstAddr) -> AllowPolicy {
        let port = dst.port();

        // First, check if the port is marked as opaque.
        if let Some((ref opaque_ranges, ref opaque_policy)) = self.opaque_ports {
            if opaque_ranges.contains(&port) {
                tracing::trace!(%port, "using opaque port policy");
                let server = Cached::uncached(opaque_policy.clone());
                return AllowPolicy { dst, server };
            }
        }

        // Lookup the polcify for the target port in the cache. If it doesn't
        // already exist, we spawn a watch on the API (if it is configured). If
        // no discovery API is configured we use the default policy.
        let server = self
            .cache
            .get_or_insert_with(port, |port| match self.discover.clone() {
                Some(disco) => info_span!("watch", port).in_scope(|| {
                    tracing::trace!(%port, "spawning policy discovery");
                    disco.spawn_with_init(*port, self.default_rx.borrow().clone())
                }),

                // If no discovery API is configured, then we use the
                // default policy. Whlie it's a little wasteful to cache
                // these results separately, this case isn't expected to be
                // used outside of testing.
                None => {
                    tracing::trace!(%port, "using the default policy");
                    self.default_rx.clone()
                }
            });

        AllowPolicy { dst, server }
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
