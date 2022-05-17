use super::{api, AllowPolicy, CheckPolicy, DefaultPolicy, DeniedUnknownPort};
use linkerd_app_core::{proxy::http, transport::OrigDstAddr, Error, Result};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use parking_lot::Mutex;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use tokio::sync::watch;
use tracing::info_span;

#[derive(Clone)]
pub enum Store<S> {
    Dynamic(Dynamic<S>),
    Fixed(Fixed),
}

#[derive(Clone)]
pub struct Dynamic<S> {
    default: DefaultPolicy,
    ports: Arc<Mutex<PortMap<Rx>>>,
    watch: api::Watch<S>,
}

/// Used primarily for testing.
///
/// It can also be used when the policy controller is not configured on a proxy
/// (i.e. when the proxy is configured by an older injector).
#[derive(Clone)]
pub struct Fixed {
    // When None, the default policy is 'deny'.
    default_rx: Option<Rx>,
    ports: Arc<PortMap<Rx>>,
}

type Tx = watch::Sender<ServerPolicy>;
type Rx = watch::Receiver<ServerPolicy>;

/// A `HashMap` optimized for lookups by port number.
type PortMap<T> = HashMap<u16, T, BuildHasherDefault<PortHasher>>;

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

// === impl Store ===

impl<S> Store<S> {
    pub(crate) fn fixed(
        default: impl Into<DefaultPolicy>,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> (Self, Option<Tx>) {
        let (fixed, default) = Fixed::new(default, ports);
        (Self::Fixed(fixed), default)
    }

    /// Spawns a watch for each of the given ports.
    ///
    /// The returned future completes when a watch has been successfully created for all of the
    /// provided ports. The store maintains these watches so that each time a policy is checked, it
    /// may obtain the latest policy provided by the watch. An error is returned if any of the
    /// watches cannot be established.
    pub(super) fn spawn_discover(
        default: DefaultPolicy,
        ports: HashSet<u16>,
        watch: api::Watch<S>,
    ) -> Self
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody:
            http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
    {
        let rxs = ports
            .into_iter()
            .map(|port| {
                let watch = watch.clone();
                let default = default.clone();
                let rx = info_span!("watch", %port)
                    .in_scope(|| watch.spawn_with_init(port, default.into()));
                (port, rx)
            })
            .collect::<PortMap<_>>();

        Self::Dynamic(Dynamic {
            default,
            watch,
            ports: Arc::new(Mutex::new(rxs)),
        })
    }
}

impl<S> CheckPolicy for Store<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    #[inline]
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        match self {
            Self::Dynamic(dynamic) => dynamic.check_policy(dst),
            Self::Fixed(fixed) => fixed.check_policy(dst),
        }
    }
}

// === impl Dynamic ===

impl<S> CheckPolicy for Dynamic<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
    S: Clone + Send + Sync + 'static,
    S::Future: Send,
    S::ResponseBody:
        http::HttpBody<Data = tonic::codegen::Bytes, Error = Error> + Default + Send + 'static,
{
    /// Checks the policy for the given destination port.
    ///
    /// If the destination has not already been discovered, then we spawn a new
    /// watch for the target port, caching the watch for the lifetime of the
    /// process. The process's default policy is used for the initial state of
    /// unknown watches.
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        let port = dst.port();
        let mut ports = self.ports.lock();
        let server = match ports.entry(port) {
            Entry::Occupied(ent) => ent.get().clone(),
            Entry::Vacant(ent) => {
                let default = self.default.clone();
                let watch = self.watch.clone();
                let rx = info_span!("watch", %port)
                    .in_scope(|| watch.spawn_with_init(port, default.into()));
                // TODO(ver): We should evict dynamically watched ports after an
                // idle timeout.
                ent.insert(rx).clone()
            }
        };
        Ok(AllowPolicy { dst, server })
    }
}

// === impl Fixed ===

impl Fixed {
    pub(crate) fn new(
        default: impl Into<DefaultPolicy>,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> (Self, Option<Tx>) {
        let default = default.into();
        let rxs = ports
            .into_iter()
            .map(|(p, s)| {
                // When using a fixed policy, we don't need to watch for changes. It's
                // safe to discard the sender, as the receiver will continue to let us
                // borrow/clone each fixed policy.
                let (_, rx) = watch::channel(s);
                (p, rx)
            })
            .collect();

        let (default_tx, default_rx) = match Self::mk_default(default) {
            Some((tx, rx)) => (Some(tx), Some(rx)),
            None => (None, None),
        };

        let store = Self {
            default_rx,
            ports: Arc::new(rxs),
        };
        (store, default_tx)
    }

    fn mk_default(default: DefaultPolicy) -> Option<(Tx, Rx)> {
        match default {
            DefaultPolicy::Deny => None,
            DefaultPolicy::Allow(sp) => Some(watch::channel(sp)),
        }
    }
}

impl CheckPolicy for Fixed {
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        let port = dst.port();
        let server = self
            .ports
            .get(&port)
            .cloned()
            .or_else(|| self.default_rx.clone())
            .ok_or(DeniedUnknownPort(port))?;
        Ok(AllowPolicy { dst, server })
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
