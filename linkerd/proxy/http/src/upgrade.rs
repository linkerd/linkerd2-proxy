//! HTTP/1.1 Upgrades

use crate::BoxBody;
use bytes::Bytes;
use futures::{
    future::{self, Either},
    TryFutureExt,
};
use hyper::{body::HttpBody, upgrade::OnUpgrade};
use linkerd_duplex::Duplex;
use pin_project::{pin_project, pinned_drop};
use std::{
    fmt, mem,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, info, instrument::Instrument, trace};
use try_lock::TryLock;

#[derive(Debug)]
pub struct SetupHttp11Connect<S> {
    service: S,
    /// Watch any spawned HTTP/1.1 upgrade tasks.
    upgrade_drain_signal: drain::Watch,
}

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

/// Provides optional HTTP/1.1 upgrade support on the body.
#[pin_project(PinnedDrop)]
#[derive(Debug)]
pub struct UpgradeBody {
    /// In UpgradeBody::drop, if this was an HTTP upgrade, the body is taken
    /// to be inserted into the Http11Upgrade half.
    body: hyper::Body,
    pub(super) upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
}

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

// === impl Http11Upgrade ===

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

// === impl SetupHttp11Connect ===

impl<S> SetupHttp11Connect<S> {
    pub fn new(service: S, upgrade_drain_signal: drain::Watch) -> Self {
        Self {
            service,
            upgrade_drain_signal,
        }
    }
}

type ResponseFuture<F, B, E> = Either<F, future::Ready<Result<http::Response<B>, E>>>;

impl<S, B> tower::Service<http::Request<hyper::Body>> for SetupHttp11Connect<S>
where
    S: tower::Service<http::Request<BoxBody>, Response = http::Response<B>>,
    B: Default,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, B, S::Error>;

    #[inline]
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

        let req = req.map(|body| BoxBody::new(UpgradeBody::new(body, upgrade)));

        Either::Left(self.service.call(req))
    }
}

/// Returns if the received request is definitely bad.
///
/// Just because a request parses doesn't mean it's correct. For examples:
///
/// - `GET example.com`
/// - `CONNECT /just-a-path
fn is_bad_request<B>(req: &http::Request<B>) -> bool {
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

/// Checks requests to determine if they want to perform an HTTP upgrade.
pub(crate) fn wants_upgrade<B>(req: &http::Request<B>) -> bool {
    // HTTP upgrades were added in 1.1, not 1.0.
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

// === impl UpgradeBody ===

impl HttpBody for UpgradeBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let body = self.project().body;
        let poll = futures::ready!(Pin::new(body) // `hyper::Body` is Unpin
            .poll_data(cx));
        Poll::Ready(poll.map(|x| {
            x.map_err(|e| {
                debug!("http body error: {}", e);
                e
            })
        }))
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let body = self.project().body;
        Pin::new(body) // `hyper::Body` is Unpin
            .poll_trailers(cx)
            .map_err(|e| {
                debug!("http trailers error: {}", e);
                e
            })
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Default for UpgradeBody {
    fn default() -> Self {
        hyper::Body::empty().into()
    }
}

impl From<hyper::Body> for UpgradeBody {
    fn from(body: hyper::Body) -> Self {
        Self {
            body,
            upgrade: None,
        }
    }
}

impl UpgradeBody {
    pub(crate) fn new(
        body: hyper::Body,
        upgrade: Option<(Http11Upgrade, hyper::upgrade::OnUpgrade)>,
    ) -> Self {
        Self { body, upgrade }
    }
}

#[pinned_drop]
impl PinnedDrop for UpgradeBody {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        // If an HTTP/1 upgrade was wanted, send the upgrade future.
        if let Some((upgrade, on_upgrade)) = this.upgrade.take() {
            upgrade.insert_half(on_upgrade);
        }
    }
}
