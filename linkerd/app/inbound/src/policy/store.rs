use super::{api, AllowPolicy, DefaultPolicy, GetPolicy};
use linkerd_app_core::{proxy::http, transport::OrigDstAddr, Error};
use linkerd_cache::Cache;
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, Hasher},
};
use tokio::{sync::watch, time::Duration};
use tracing::info_span;

#[derive(Clone)]
pub struct Store<S> {
    cache: Cache<u16, Rx, BuildHasherDefault<PortHasher>>,
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
            Cache::with_permanent_from_iter(Duration::MAX, rxs)
        };

        let (default_tx, default_rx) = watch::channel(ServerPolicy::from(default));
        tokio::spawn(async move {
            default_tx.closed().await;
        });

        Self {
            cache,
            default_rx,
            discover: None,
        }
    }

    /// Spawns a watch for each of the given ports.
    ///
    /// The returned future completes when a watch has been successfully created
    /// for all of the provided ports. The store maintains these watches so that
    /// each time a policy is checked, it may obtain the latest policy provided
    /// by the watch. An error is returned if any of the watches cannot be
    /// established.
    pub(super) fn spawn_discover(
        default: DefaultPolicy,
        ports: HashSet<u16>,
        watch: api::Watch<S>,
        idle_timeout: Duration,
    ) -> Self
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody:
            http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    {
        let (default_tx, default_rx) = watch::channel(ServerPolicy::from(default));
        tokio::spawn(async move {
            default_tx.closed().await;
        });

        // The initial set of documented policies never expire from the cache.
        //
        // Policies that are dynamically discovered at runtime will expire after
        // `idle_timeout` to prevent holding policy watches indefinitely for
        // ports that are generally unused.
        let cache = {
            let rxs = ports.into_iter().map(|port| {
                let watch = watch.clone();
                let default = default_rx.borrow().clone();
                let rx =
                    info_span!("watch", %port).in_scope(|| watch.spawn_with_init(port, default));
                (port, rx)
            });
            Cache::with_permanent_from_iter(idle_timeout, rxs)
        };

        Self {
            cache,
            default_rx,
            discover: Some(watch),
        }
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
        // Lookup the polcify for the target port in the cache. If it doesn't
        // already exist, we spawn a watch on the API (if it is configured). If
        // no discovery API is configured we use the default policy.
        let server =
            self.cache
                .get_or_insert_with(dst.port(), |port| match self.discover.clone() {
                    Some(disco) => info_span!("watch", %port).in_scope(|| {
                        disco.spawn_with_init(*port, self.default_rx.borrow().clone())
                    }),
                    None => self.default_rx.clone(),
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
