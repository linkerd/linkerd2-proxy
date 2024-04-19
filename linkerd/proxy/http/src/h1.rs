use crate::{
    glue::HyperConnect,
    upgrade::{Http11Upgrade, HttpConnect},
};
use futures::prelude::*;
use http::{
    header::{CONTENT_LENGTH, TRANSFER_ENCODING},
    uri::Uri,
};
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_stack::MakeConnection;
use std::{pin::Pin, time::Duration};
use tracing::{debug, trace};

#[derive(Copy, Clone, Debug)]
pub struct WasAbsoluteForm(pub(crate) ());

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct PoolSettings {
    pub max_idle: usize,
    pub idle_timeout: Duration,
}

/// Communicates with HTTP/1.x servers.
///
/// The client handles both absolute-form and origin-form requests by lazily
/// building a client for each form, as needed. This is needed because Hyper
/// requires that this is configured at the connection level; so we can possibly
/// end up maintaining two independent connection pools, if needed.
#[derive(Debug)]
pub struct Client<C, T, B> {
    connect: C,
    target: T,
    absolute_form: Option<hyper::Client<HyperConnect<C, T>, B>>,
    origin_form: Option<hyper::Client<HyperConnect<C, T>, B>>,
    pool: PoolSettings,
}

impl<C, T, B> Client<C, T, B> {
    pub fn new(connect: C, target: T, pool: PoolSettings) -> Self {
        Self {
            connect,
            target,
            absolute_form: None,
            origin_form: None,
            pool,
        }
    }
}

impl<C: Clone, T: Clone, B> Clone for Client<C, T, B> {
    fn clone(&self) -> Self {
        Self {
            connect: self.connect.clone(),
            target: self.target.clone(),
            absolute_form: self.absolute_form.clone(),
            origin_form: self.origin_form.clone(),
            pool: self.pool,
        }
    }
}

type RspFuture = Pin<Box<dyn Future<Output = Result<http::Response<BoxBody>>> + Send + 'static>>;

impl<C, T, B> Client<C, T, B>
where
    T: Clone + Send + Sync + 'static,
    C: MakeConnection<(crate::Version, T)> + Clone + Send + Sync + 'static,
    C::Connection: Unpin + Send,
    C::Future: Unpin + Send + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    pub(crate) fn request(&mut self, mut req: http::Request<B>) -> RspFuture {
        // Marked by `upgrade`.
        let upgrade = req.extensions_mut().remove::<Http11Upgrade>();
        let is_http_connect = req.method() == http::Method::CONNECT;

        // Configured by `normalize_uri` or `orig_proto::Downgrade`.
        let use_absolute_form = req.extensions_mut().remove::<WasAbsoluteForm>().is_some();
        debug_assert!(req.uri().authority().is_some());

        let is_missing_host = req
            .headers()
            .get(http::header::HOST)
            .map(|v| v.is_empty())
            .unwrap_or(true);

        let rsp_fut = if req.version() == http::Version::HTTP_10 || is_missing_host {
            // If there's no authority, we assume we're on some weird HTTP/1.0
            // ish, so we just build a one-off client for the connection.
            // There's no real reason to hold the client for re-use.
            debug!(use_absolute_form, is_missing_host, "Using one-off client");
            hyper::Client::builder()
                .pool_max_idle_per_host(0)
                .set_host(use_absolute_form)
                .build(HyperConnect::new(
                    self.connect.clone(),
                    self.target.clone(),
                    use_absolute_form,
                ))
                .request(req)
        } else {
            // Otherwise, use a cached client to take advantage of the
            // connection pool. The client needs to be configured for absolute
            // (HTTP proxy-style) URIs, so we cache separate absolute/origin
            // clients lazily.
            let client = if use_absolute_form {
                trace!("Using absolute-form client");
                &mut self.absolute_form
            } else {
                trace!("Using origin-form client");
                &mut self.origin_form
            };

            if client.is_none() {
                debug!(use_absolute_form, "Caching new client");
                *client = Some(
                    hyper::Client::builder()
                        .pool_max_idle_per_host(self.pool.max_idle)
                        .pool_idle_timeout(self.pool.idle_timeout)
                        .set_host(use_absolute_form)
                        .build(HyperConnect::new(
                            self.connect.clone(),
                            self.target.clone(),
                            use_absolute_form,
                        )),
                );
            }

            client.as_ref().unwrap().request(req)
        };

        Box::pin(rsp_fut.err_into().map_ok(move |mut rsp| {
            if is_http_connect {
                debug_assert!(
                    upgrade.is_some(),
                    "Upgrade extension must be set on CONNECT requests"
                );
                rsp.extensions_mut().insert(HttpConnect);

                // Strip headers that may not be transmitted to the server, per
                // https://tools.ietf.org/html/rfc7231#section-4.3.6:
                //
                // A client MUST ignore any Content-Length or Transfer-Encoding
                // header fields received in a successful response to CONNECT.
                if rsp.status().is_success() {
                    rsp.headers_mut().remove(CONTENT_LENGTH);
                    rsp.headers_mut().remove(TRANSFER_ENCODING);
                }
            }

            if is_upgrade(&rsp) {
                trace!("Client response is HTTP/1.1 upgrade");
                if let Some(upgrade) = upgrade {
                    upgrade.insert_half(hyper::upgrade::on(&mut rsp));
                }
            } else {
                crate::strip_connection_headers(rsp.headers_mut());
            }

            rsp.map(BoxBody::new)
        }))
    }
}

/// Checks responses to determine if they are successful HTTP upgrades.
pub(crate) fn is_upgrade<B>(res: &http::Response<B>) -> bool {
    #[inline]
    fn is_connect_success<B>(res: &http::Response<B>) -> bool {
        res.extensions().get::<HttpConnect>().is_some() && res.status().is_success()
    }

    // Upgrades were introduced in HTTP/1.1
    if res.version() != http::Version::HTTP_11 {
        if is_connect_success(res) {
            tracing::warn!(
                "A successful response to a CONNECT request had an incorrect HTTP version \
                (expected HTTP/1.1, got {:?})",
                res.version()
            );
        }
        return false;
    }

    // 101 Switching Protocols
    if res.status() == http::StatusCode::SWITCHING_PROTOCOLS {
        return true;
    }

    // CONNECT requests are complete if status code is 2xx.
    if is_connect_success(res) {
        return true;
    }

    // Just a regular HTTP response...
    false
}

/// Returns if the request target is in `absolute-form`.
///
/// This is `absolute-form`: `https://example.com/docs`
///
/// This is not:
///
/// - `/docs`
/// - `example.com`
pub(crate) fn is_absolute_form(uri: &Uri) -> bool {
    // It's sufficient just to check for a scheme, since in HTTP1,
    // it's required in absolute-form, and `http::Uri` doesn't
    // allow URIs with the other parts missing when the scheme is set.
    debug_assert!(
        uri.scheme().is_none() || (uri.authority().is_some() && uri.path_and_query().is_some()),
        "is_absolute_form http::Uri invariants: {:?}",
        uri
    );

    uri.scheme().is_some()
}

/// Returns if the request target is in `origin-form`.
///
/// This is `origin-form`: `example.com`
fn is_origin_form(uri: &Uri) -> bool {
    uri.scheme().is_none() && uri.path_and_query().is_none()
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
