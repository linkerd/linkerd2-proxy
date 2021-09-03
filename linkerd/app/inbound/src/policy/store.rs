use super::{discover, AllowPolicy, CheckPolicy, DefaultPolicy, DeniedUnknownPort};
use futures::prelude::*;
use linkerd_app_core::{proxy::http, transport::OrigDstAddr, Error, Result};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use tokio::sync::watch;
use tracing::{info_span, Instrument};

#[derive(Clone, Debug)]
pub struct Store {
    // When None, the default policy is 'deny'.
    default: Option<Rx>,
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

impl Store {
    fn mk_default(default: DefaultPolicy) -> Option<(Tx, Rx)> {
        match default {
            DefaultPolicy::Deny => None,
            DefaultPolicy::Allow(sp) => Some(watch::channel(sp)),
        }
    }

    pub(crate) fn fixed(
        default: impl Into<DefaultPolicy>,
        ports: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> (Self, Option<Tx>) {
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

        let (default_tx, default) = match Self::mk_default(default.into()) {
            Some((tx, rx)) => (Some(tx), Some(rx)),
            None => (None, None),
        };

        let store = Self {
            default,
            ports: Arc::new(rxs),
        };
        (store, default_tx)
    }

    /// Spawns a watch for each of the given ports.
    ///
    /// The returned future completes when a watch has been successfully created for all of the
    /// provided ports. The store maintains these watches so that each time a policy is checked, it
    /// may obtain the latest policy provided by the watch. An error is returned if any of the
    /// watches cannot be established.
    //
    // XXX(ver): rustc can't seem to figure out that this Future is `Send` unless we annotate it
    // explicitly, hence the manual_async_fn.
    #[allow(clippy::manual_async_fn)]
    pub(super) fn spawn_discover<S>(
        default: DefaultPolicy,
        ports: HashSet<u16>,
        discover: discover::Watch<S>,
    ) -> impl Future<Output = Result<Self>> + Send
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
    {
        async move {
            let rxs = ports.into_iter().map(|port| {
                discover
                    .clone()
                    .spawn_watch(port)
                    .instrument(info_span!("watch", %port))
                    .map_ok(move |rsp| (port, rsp.into_inner()))
            });

            let ports = futures::future::join_all(rxs)
                .await
                .into_iter()
                .collect::<Result<PortMap<_>, tonic::Status>>()?;

            let default = match Self::mk_default(default) {
                Some((tx, rx)) => {
                    tokio::spawn(async move {
                        tx.closed().await;
                    });
                    Some(rx)
                }
                None => None,
            };

            Ok(Self {
                default,
                ports: Arc::new(ports),
            })
        }
    }
}

impl CheckPolicy for Store {
    /// Checks that the destination port is configured to allow traffic.
    ///
    /// If the port is not explicitly configured, then the default policy is used. If the default
    /// policy is `deny`, then a `DeniedUnknownPort` error is returned; otherwise an `AllowPolicy`
    /// is returned that can be used to check whether the connection is permitted via
    /// [`AllowPolicy::check_authorized`].
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        let server = self
            .ports
            .get(&dst.port())
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| match &self.default {
                Some(rx) => Ok(rx.clone()),
                None => Err(DeniedUnknownPort(dst.port())),
            })?;

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
