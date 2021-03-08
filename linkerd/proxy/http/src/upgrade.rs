//! HTTP/1.1 Upgrades

use crate::{glue::UpgradeBody, h1};
use futures::{
    future::{self, Either},
    TryFutureExt,
};
use hyper::upgrade::OnUpgrade;
use linkerd_drain as drain;
use linkerd_duplex::Duplex;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::instrument::Instrument;
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
    pub fn halves(upgrade_drain_signal: drain::Watch) -> Http11UpgradeHalves {
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
                inner,
            },
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

            let both_upgrades = async move {
                let (server_conn, client_conn) = tokio::try_join!(server_upgrade, client_upgrade)?;
                trace!("HTTP upgrade successful");
                if let Err(e) = Duplex::new(client_conn, server_conn).await {
                    info!("tcp duplex error: {}", e)
                }
                Ok::<(), ()>(())
            };
            // There's nothing to do when drain is signaled, we just have to hope
            // the sockets finish soon. However, the drain signal still needs to
            // 'watch' the TCP future so that the process doesn't close early.
            tokio::spawn(
                self.upgrade_drain_signal
                    .take()
                    .expect("only taken in drop")
                    .ignore_signaled()
                    .release_after(both_upgrades)
                    .in_current_span(),
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

type ResponseFuture<F, B, E> = Either<F, future::Ready<Result<http::Response<B>, E>>>;

impl<S, B> tower::Service<http::Request<hyper::Body>> for Service<S>
where
    S: tower::Service<http::Request<UpgradeBody>, Response = http::Response<B>>,
    B: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, B, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<hyper::Body>) -> Self::Future {
        // Should this rejection happen later in the Service stack?
        //
        // Rejecting here means telemetry doesn't record anything about it...
        //
        // At the same time, this stuff is specifically HTTP1, so it feels
        // proper to not have the HTTP2 requests going through it...
        if h1::is_bad_request(&req) {
            let mut res = http::Response::default();
            *res.status_mut() = http::StatusCode::BAD_REQUEST;
            return Either::Right(future::ok(res));
        }

        let upgrade = if h1::wants_upgrade(&req) {
            trace!("server request wants HTTP/1.1 upgrade");
            // Upgrade requests include several "connection" headers that
            // cannot be removed.

            // Setup HTTP Upgrade machinery.
            let halves = Http11Upgrade::halves(self.upgrade_drain_signal.clone());
            req.extensions_mut().insert(halves.client);
            let on_upgrade = hyper::upgrade::on(&mut req);

            Some((halves.server, on_upgrade))
        } else {
            h1::strip_connection_headers(req.headers_mut());
            None
        };

        let req = req.map(|body| UpgradeBody::new(body, upgrade));

        Either::Left(self.service.call(req))
    }
}
