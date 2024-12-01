use super::{api, AllowPolicy, DefaultPolicy, GetPolicy};
use linkerd_app_core::{proxy::http, transport::OrigDstAddr, Error};
use linkerd_idle_cache::IdleCache;
pub use linkerd_proxy_server_policy::{Protocol, ServerPolicy};
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
    cache: IdleCache<u16, Rx, BuildHasherDefault<PortHasher>>,
    default_rx: Rx,
    opaque_ports: Arc<RangeInclusiveSet<u16>>,
    opaque_default_rx: Rx,
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
        opaque_ports: RangeInclusiveSet<u16>,
    ) -> Self {
        let opaque_default_rx = Self::spawn_default(Self::make_opaque(default.clone()));
        let cache = {
            let opaque_rxs = opaque_ports
                .iter()
                .flat_map(|range| range.clone().map(|port| (port, opaque_default_rx.clone())));
            let rxs = ports.into_iter().map(|(p, s)| {
                // When using a fixed policy, we don't need to watch for changes. It's
                // safe to discard the sender, as the receiver will continue to let us
                // borrow/clone each fixed policy.
                let (_, rx) = watch::channel(s);
                (p, rx)
            });
            IdleCache::with_permanent_from_iter(idle_timeout, opaque_rxs.chain(rxs))
        };

        Self {
            cache,
            discover: None,
            default_rx: Self::spawn_default(default),
            opaque_ports: Arc::new(opaque_ports),
            opaque_default_rx,
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
        opaque_ports: RangeInclusiveSet<u16>,
    ) -> Self
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody:
            http::Body<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    {
        let opaque_default = Self::make_opaque(default.clone());
        // The initial set of policies never expire from the cache.
        //
        // Policies that are dynamically discovered at runtime will expire after
        // `idle_timeout` to prevent holding policy watches indefinitely for
        // ports that are generally unused.
        let cache = {
            let rxs = ports.into_iter().map(|port| {
                let discover = discover.clone();
                let default = if opaque_ports.contains(&port) {
                    opaque_default.clone()
                } else {
                    default.clone()
                };
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
            opaque_ports: Arc::new(opaque_ports),
            opaque_default_rx: Self::spawn_default(opaque_default),
        }
    }

    fn make_opaque(default: DefaultPolicy) -> DefaultPolicy {
        match default {
            DefaultPolicy::Allow(mut policy) => {
                policy.protocol = match policy.protocol {
                    Protocol::Detect { tcp_authorizations, .. } => {
                        Protocol::Opaque(tcp_authorizations)
                    }
                    opaq @ Protocol::Opaque(_) => opaq,
                    _ => unreachable!("default policy must have been configured to detect prior to marking it opaque"),
                };
                DefaultPolicy::Allow(policy)
            }
            DefaultPolicy::Deny => DefaultPolicy::Deny,
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
        http::Body<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    fn get_policy(&self, dst: OrigDstAddr) -> AllowPolicy {
        // Lookup the policy for the target port in the cache. If it doesn't
        // already exist, we spawn a watch on the API (if it is configured). If
        // no discovery API is configured we use the default policy.
        let server =
            self.cache
                .get_or_insert_with(dst.port(), |port| match self.discover.clone() {
                    Some(disco) => info_span!("watch", port).in_scope(|| {
                        let is_default_opaque = self.opaque_ports.contains(port);
                        tracing::trace!(%port, is_default_opaque, "spawning policy discovery");
                        // If the port is in the range of ports marked as
                        // opaque, use the opaque default policy instead.
                        let init = if is_default_opaque {
                            self.opaque_default_rx.borrow().clone()
                        } else {
                            self.default_rx.borrow().clone()
                        };
                        disco.spawn_with_init(*port, init)
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
