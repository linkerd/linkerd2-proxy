//! HTTP/1.1 Upgrades

use crate::glue::UpgradeBody;
use futures::{
    future::{self, Either},
    TryFutureExt,
};
use hyper::upgrade::OnUpgrade;
use linkerd_duplex::Duplex;
use std::{
    fmt, mem,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::instrument::Instrument;
use tracing::{debug, info, trace};
use try_lock::TryLock;

/// A type inserted into `http::Extensions` to bridge together HTTP Upgrades.
///
/// If the HTTP1 server service detects an upgrade request, this will be
/// inserted into the `Request::extensions()`. If the HTTP1 client service
/// also detects an upgrade, the two `OnUpgrade` futures will be joined
/// together with the glue in this type.
// Note: this relies on there only having been 2 Inner clones, so don't
// implement `Clone` for this type.
pub struct Http11Upgrade {
    half: Half,
    inner: Arc<Inner>,
}

/// A named "tuple" returned by [`Http11Upgade::halves()`] of the two halves of
/// an upgrade.
#[derive(Debug)]
struct Http11UpgradeHalves {
    /// The "server" half.
    server: Http11Upgrade,
    /// The "client" half.
    client: Http11Upgrade,
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

// === impl Http11Upgrade ===

impl Http11Upgrade {
    /// Returns a pair of upgrade handles.
    ///
    /// Each handle is used to insert 1 half of the upgrade. When both handles
    /// have inserted, the upgrade future will be spawned onto the executor.
    fn halves(upgrade_drain_signal: drain::Watch) -> Http11UpgradeHalves {
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
        match self {
            Self {
                inner,
                half: Half::Server,
            } => {
                let mut lock = inner
                    .server
                    .try_lock()
                    .expect("only Half::Server touches server TryLock");
                debug_assert!(lock.is_none());
                *lock = Some(upgrade);
            }
            Self {
                inner,
                half: Half::Client,
            } => {
                let mut lock = inner
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

// === impl Service ===

impl<S> Service<S> {
    pub fn new(service: S, upgrade_drain_signal: drain::Watch) -> Self {
        Self {
            service,
            upgrade_drain_signal,
        }
    }
}

type ResponseFuture<F, B, E> = Either<F, future::Ready<Result<http::Response<B>, E>>>;

impl<S, ReqB, RespB> tower::Service<http::Request<ReqB>> for Service<S>
where
    S: tower::Service<http::Request<UpgradeBody<ReqB>>, Response = http::Response<RespB>>,
    RespB: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, RespB, S::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<ReqB>) -> Self::Future {
        // Should this rejection happen later in the Service stack?
        //
        // Rejecting here means telemetry doesn't record anything about it...
        //
        // At the same time, this stuff is specifically HTTP1, so it feels
        // proper to not have the HTTP2 requests going through it...
        if is_bad_request(&req) {
            let mut res = http::Response::default();
            *res.status_mut() = http::StatusCode::BAD_REQUEST;
            return Either::Right(future::ok(res));
        }

        let upgrade = if wants_upgrade(&req) {
            trace!("server request wants HTTP/1.1 upgrade");
            // Upgrade requests include several "connection" headers that
            // cannot be removed.

            // Setup HTTP Upgrade machinery.
            let halves = Http11Upgrade::halves(self.upgrade_drain_signal.clone());
            req.extensions_mut().insert(halves.client);
            let on_upgrade = hyper::upgrade::on(&mut req);

            Some((halves.server, on_upgrade))
        } else {
            crate::strip_connection_headers(req.headers_mut());
            None
        };

        let req = req.map(|body| UpgradeBody::new(body, upgrade));

        Either::Left(self.service.call(req))
    }
}

/// Returns if the received request is definitely bad.
///
/// Just because a request parses doesn't mean it's correct. For examples:
///
/// - `GET example.com`
/// - `CONNECT /just-a-path
pub(crate) fn is_bad_request<B>(req: &http::Request<B>) -> bool {
    if req.method() == http::Method::CONNECT {
        // CONNECT is only valid over HTTP/1.1
        if req.version() != http::Version::HTTP_11 {
            debug!("CONNECT request not valid for HTTP/1.0: {:?}", req.uri());
            return true;
        }

        // CONNECT requests are only valid in authority-form.
        if !is_origin_form(req.uri()) {
            debug!("CONNECT request with illegal URI: {:?}", req.uri());
            return true;
        }
    } else if is_origin_form(req.uri()) {
        // If not CONNECT, refuse any origin-form URIs
        debug!("{} request with illegal URI: {:?}", req.method(), req.uri());
        return true;
    }

    false
}

/// Returns if the request target is in `origin-form`.
///
/// This is `origin-form`: `example.com`
fn is_origin_form(uri: &http::uri::Uri) -> bool {
    uri.scheme().is_none() && uri.path_and_query().is_none()
}

/// Returns true if the given [Request<B>][http::Request] is attempting an HTTP/1.1 upgrade.
fn wants_upgrade<B>(req: &http::Request<B>) -> bool {
    // Upgrades are specific to HTTP/1.1. They are not included in HTTP/1.0, nor are they supported
    // in HTTP/2. If this request is associated with any protocol version besides HTTP/1.1, we can
    // dismiss it immediately as not being applicable to an upgrade.
    if req.version() != http::Version::HTTP_11 {
        return false;
    }

    if let Some(upgrade) = req.headers().get(http::header::UPGRADE) {
        // If an `h2` upgrade over HTTP/1.1 were to go by the proxy,
        // and it succeeded, there would an h2 connection, but it would
        // be opaque-to-the-proxy, acting as just a TCP proxy.
        //
        // A user wouldn't be able to see any usual HTTP telemetry about
        // requests going over that connection. Instead of that confusion,
        // the proxy strips h2 upgrade headers.
        //
        // Eventually, the proxy will support h2 upgrades directly.
        return upgrade != "h2c";
    }

    // HTTP/1.1 CONNECT requests are just like upgrades!
    req.method() == http::Method::CONNECT
}
