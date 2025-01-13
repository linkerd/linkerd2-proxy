use crate::{body::UpgradeBody, upgrade::Http11UpgradeHalves};
use futures::future::{self, Either};
use std::task::{Context, Poll};
use tracing::{debug, trace};

#[derive(Debug)]
pub struct Service<S> {
    service: S,
    /// Watch any spawned HTTP/1.1 upgrade tasks.
    upgrade_drain_signal: drain::Watch,
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
            let Http11UpgradeHalves { server, client } =
                crate::upgrade::halves(self.upgrade_drain_signal.clone());
            req.extensions_mut().insert(client);
            let on_upgrade = hyper::upgrade::on(&mut req);

            Some((server, on_upgrade))
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

/// Checks requests to determine if they want to perform an HTTP upgrade.
fn wants_upgrade<B>(req: &http::Request<B>) -> bool {
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
