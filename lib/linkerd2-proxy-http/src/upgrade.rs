//! HTTP/1.1 Upgrades
use super::{glue::HttpBody, h1};
use futures::{
    future::{self, Either},
    Future, Poll,
};
use hyper::upgrade::OnUpgrade;
use linkerd2_drain as drain;
use linkerd2_duplex::Duplex;
use std::fmt;
use std::mem;
use std::sync::Arc;
use tracing::{debug, info, trace};
use try_lock::TryLock;

/// A type inserted into `http::Extensions` to bridge together HTTP Upgrades.
///
/// If the HTTP1 server service detects an upgrade request, this will be
/// inserted into the `Request::extensions()`. If the HTTP1 client service
/// also detects an upgrade, the two `OnUpgrade` futures will be joined
/// together with the glue in this type.
// Note: this relies on their only having been 2 Inner clones, so don't
// implement `Clone` for this type.
pub struct Http11Upgrade {
    half: Half,
    inner: Arc<Inner>,
}

/// A named "tuple" returned by `Http11Upgade::new()` of the two halves of
/// an upgrade.
#[derive(Debug)]
pub struct Http11UpgradeHalves {
    /// The "server" half.
    pub server: Http11Upgrade,
    /// The "client" half.
    pub client: Http11Upgrade,
    _inner: (),
}

/// A marker type inserted into Extensions to signal it was an HTTP CONNECT
/// request.
#[derive(Debug)]
pub struct HttpConnect;

struct Inner {
    server: TryLock<Option<OnUpgrade>>,
    client: TryLock<Option<OnUpgrade>>,
    upgrade_drain_signal: Option<drain::Watch>,
}

#[derive(Debug)]
enum Half {
    Server,
    Client,
}

#[derive(Debug)]
pub struct Service<S> {
    service: S,
    /// Watch any spawned HTTP/1.1 upgrade tasks.
    upgrade_drain_signal: drain::Watch,
}

// ===== impl Http11Upgrade =====

impl Http11Upgrade {
    /// Returns a pair of upgrade handles.
    ///
    /// Each handle is used to insert 1 half of the upgrade. When both handles
    /// have inserted, the upgrade future will be spawned onto the executor.
    pub fn new(upgrade_drain_signal: drain::Watch) -> Http11UpgradeHalves {
        let inner = Arc::new(Inner {
            server: TryLock::new(None),
            client: TryLock::new(None),
            upgrade_drain_signal: Some(upgrade_drain_signal),
        });

        Http11UpgradeHalves {
            server: Http11Upgrade {
                half: Half::Server,
                inner: inner.clone(),
            },
            client: Http11Upgrade {
                half: Half::Client,
                inner: inner,
            },
            _inner: (),
        }
    }

    pub fn insert_half(self, upgrade: OnUpgrade) {
        match self.half {
            Half::Server => {
                let mut lock = self
                    .inner
                    .server
                    .try_lock()
                    .expect("only Half::Server touches server TryLock");
                debug_assert!(lock.is_none());
                *lock = Some(upgrade);
            }
            Half::Client => {
                let mut lock = self
                    .inner
                    .client
                    .try_lock()
                    .expect("only Half::Client touches client TryLock");
                debug_assert!(lock.is_none());
                *lock = Some(upgrade);
            }
        }
    }
}

impl fmt::Debug for Http11Upgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http11Upgrade")
            .field("half", &self.half)
            .finish()
    }
}

/// When both halves have dropped, check if both sides are inserted,
/// and if so, spawn the upgrade task.
impl Drop for Inner {
    fn drop(&mut self) {
        // Since this is Inner::drop, no more synchronization is required.
        // We can safely take the futures out of their locks.
        let server = mem::replace(&mut self.server, TryLock::new(None)).into_inner();
        let client = mem::replace(&mut self.client, TryLock::new(None)).into_inner();
        if let (Some(server), Some(client)) = (server, client) {
            trace!("HTTP/1.1 upgrade has both halves");

            let server_upgrade = server.map_err(|e| debug!("server HTTP upgrade error: {}", e));

            let client_upgrade = client.map_err(|e| debug!("client HTTP upgrade error: {}", e));

            let both_upgrades =
                server_upgrade
                    .join(client_upgrade)
                    .and_then(|(server_conn, client_conn)| {
                        trace!("HTTP upgrade successful");
                        Duplex::new(server_conn, client_conn)
                            .map_err(|e| info!("tcp duplex error: {}", e))
                    });

            // There's nothing to do when drain is signaled, we just have to hope
            // the sockets finish soon. However, the drain signal still needs to
            // 'watch' the TCP future so that the process doesn't close early.
            tokio::spawn(
                self.upgrade_drain_signal
                    .take()
                    .expect("only taken in drop")
                    .watch(both_upgrades, |_| ()),
            );
        } else {
            trace!("HTTP/1.1 upgrade half missing");
        }
    }
}

// ===== impl Service =====
impl<S> Service<S> {
    pub fn new(service: S, upgrade_drain_signal: drain::Watch) -> Self {
        Self {
            service,
            upgrade_drain_signal,
        }
    }
}

impl<S, B> tower::Service<http::Request<HttpBody>> for Service<S>
where
    S: tower::Service<http::Request<HttpBody>, Response = http::Response<B>>,
    B: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<S::Future, future::FutureResult<http::Response<B>, Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<HttpBody>) -> Self::Future {
        // Should this rejection happen later in the Service stack?
        //
        // Rejecting here means telemetry doesn't record anything about it...
        //
        // At the same time, this stuff is specifically HTTP1, so it feels
        // proper to not have the HTTP2 requests going through it...
        if h1::is_bad_request(&req) {
            let mut res = http::Response::default();
            *res.status_mut() = http::StatusCode::BAD_REQUEST;
            return Either::B(future::ok(res));
        }

        let upgrade = if h1::wants_upgrade(&req) {
            trace!("server request wants HTTP/1.1 upgrade");
            // Upgrade requests include several "connection" headers that
            // cannot be removed.

            // Setup HTTP Upgrade machinery.
            let halves = Http11Upgrade::new(self.upgrade_drain_signal.clone());
            req.extensions_mut().insert(halves.client);

            Some(halves.server)
        } else {
            h1::strip_connection_headers(req.headers_mut());
            None
        };

        req.body_mut().upgrade = upgrade;

        Either::A(self.service.call(req))
    }
}
