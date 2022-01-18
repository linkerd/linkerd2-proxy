use super::{h1, h2, upgrade};
use futures::{future, prelude::*};
use http::header::{HeaderValue, TRANSFER_ENCODING};
use hyper::body::HttpBody;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_stack::{layer, MakeConnection, Service};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tracing::{debug, trace, warn};

pub const L5D_ORIG_PROTO: &str = "l5d-orig-proto";

/// Upgrades HTTP requests from their original protocol to HTTP2.
#[derive(Debug)]
pub struct Upgrade<C, T, B> {
    http1: h1::Client<C, T, B>,
    h2: h2::Connection<B>,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("upgraded connection failed with HTTP/2 reset: {0}")]
pub struct DowngradedH2Error(h2::Reason);

#[pin_project::pin_project]
#[derive(Debug, Default)]
pub struct UpgradeResponseBody {
    inner: hyper::Body,
}

/// Downgrades HTTP2 requests that were previousl upgraded to their original
/// protocol.
#[derive(Clone, Debug)]
pub struct Downgrade<S> {
    inner: S,
}

// === impl Upgrade ===

impl<C, T, B> Upgrade<C, T, B> {
    pub(crate) fn new(http1: h1::Client<C, T, B>, h2: h2::Connection<B>) -> Self {
        Self { http1, h2 }
    }
}

impl<C, T, B> Service<http::Request<B>> for Upgrade<C, T, B>
where
    T: Clone + Send + Sync + 'static,
    C: MakeConnection<T> + Clone + Send + Sync + 'static,
    C::Connection: Unpin + Send,
    C::Future: Unpin + Send + 'static,
    B: hyper::body::HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<Error> + Send + Sync,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<BoxBody>>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.h2.poll_ready(cx).map_err(downgrade_h2_error)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        debug_assert!(req.version() != http::Version::HTTP_2);
        if req.extensions().get::<upgrade::Http11Upgrade>().is_some() {
            debug!("Skipping orig-proto upgrade due to HTTP/1.1 upgrade");
            return Box::pin(self.http1.request(req).map_ok(|rsp| rsp.map(BoxBody::new)));
        }

        let orig_version = req.version();
        let absolute_form = req
            .extensions_mut()
            .remove::<h1::WasAbsoluteForm>()
            .is_some();
        debug!(version = ?orig_version, absolute_form, "Upgrading request");

        // absolute-form is far less common, origin-form is the usual,
        // so only encode the extra information if it's different than
        // the normal.
        let header = match (orig_version, absolute_form) {
            (http::Version::HTTP_11, false) => "HTTP/1.1",
            (http::Version::HTTP_11, true) => "HTTP/1.1; absolute-form",
            (http::Version::HTTP_10, false) => "HTTP/1.0",
            (http::Version::HTTP_10, true) => "HTTP/1.0; absolute-form",
            (v, _) => unreachable!("bad orig-proto version: {:?}", v),
        };
        req.headers_mut()
            .insert(L5D_ORIG_PROTO, HeaderValue::from_static(header));

        // transfer-encoding is illegal in HTTP2
        req.headers_mut().remove(TRANSFER_ENCODING);

        *req.version_mut() = http::Version::HTTP_2;

        Box::pin(
            self.h2
                .call(req)
                .map_err(downgrade_h2_error)
                .map_ok(move |mut rsp| {
                    let version = rsp
                        .headers_mut()
                        .remove(L5D_ORIG_PROTO)
                        .and_then(|orig_proto| {
                            if orig_proto == "HTTP/1.1" {
                                Some(http::Version::HTTP_11)
                            } else if orig_proto == "HTTP/1.0" {
                                Some(http::Version::HTTP_10)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(orig_version);
                    trace!(?version, "Downgrading response");
                    *rsp.version_mut() = version;
                    rsp.map(|inner| BoxBody::new(UpgradeResponseBody { inner }))
                }),
        )
    }
}

/// Handles HTTP/2 client errors for HTTP/1.1 requests by wrapping the error type. This
/// simplifies error handling elsewhere so that HTTP/2 errors can only be encountered when the
/// original request was HTTP/2.
fn downgrade_h2_error<E: std::error::Error + Send + Sync + 'static>(orig: E) -> Error {
    #[inline]
    fn reason(e: &(dyn std::error::Error + 'static)) -> Option<h2::Reason> {
        e.downcast_ref::<h2::H2Error>()?.reason()
    }

    // If the provided error was an H2 error, wrap it as a downgraded error.
    if let Some(reason) = reason(&orig) {
        return DowngradedH2Error(reason).into();
    }

    // Otherwise, check the source chain to see if its original error was an H2 error.
    let mut cause = orig.source();
    while let Some(error) = cause {
        if let Some(reason) = reason(error) {
            return DowngradedH2Error(reason).into();
        }

        cause = error.source();
    }

    // If the error was not an H2 error, return the original error (boxed).
    orig.into()
}

#[cfg(test)]
#[test]
fn test_downgrade_h2_error() {
    assert!(
        downgrade_h2_error(h2::H2Error::from(h2::Reason::PROTOCOL_ERROR)).is::<DowngradedH2Error>(),
        "h2 errors must be downgraded"
    );

    #[derive(Debug, Error)]
    #[error("wrapped h2 error: {0}")]
    struct WrapError(#[source] Error);
    assert!(
        downgrade_h2_error(WrapError(
            h2::H2Error::from(h2::Reason::PROTOCOL_ERROR).into()
        ))
        .is::<DowngradedH2Error>(),
        "wrapped h2 errors must be downgraded"
    );

    assert!(
        downgrade_h2_error(WrapError(
            WrapError(h2::H2Error::from(h2::Reason::PROTOCOL_ERROR).into()).into()
        ))
        .is::<DowngradedH2Error>(),
        "double-wrapped h2 errors must be downgraded"
    );

    assert!(
        !downgrade_h2_error(std::io::Error::new(
            std::io::ErrorKind::Other,
            "non h2 error"
        ))
        .is::<DowngradedH2Error>(),
        "other h2 errors must not be downgraded"
    );
}

// === impl UpgradeResponseBody ===

impl HttpBody for UpgradeResponseBody {
    type Data = bytes::Bytes;
    type Error = Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        Pin::new(self.project().inner)
            .poll_data(cx)
            .map_err(downgrade_h2_error)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        Pin::new(self.project().inner)
            .poll_trailers(cx)
            .map_err(downgrade_h2_error)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        HttpBody::size_hint(&self.inner)
    }
}

// === impl Downgrade ===

impl<S> Downgrade<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy + Clone {
        layer::mk(|inner| Self { inner })
    }
}

type DowngradeFuture<F, T> = future::MapOk<F, fn(T) -> T>;

impl<S, A, B> Service<http::Request<A>> for Downgrade<S>
where
    S: Service<http::Request<A>, Response = http::Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = DowngradeFuture<S::Future, S::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<A>) -> Self::Future {
        let mut upgrade_response = false;

        if req.version() == http::Version::HTTP_2 {
            if let Some(orig_proto) = req.headers_mut().remove(L5D_ORIG_PROTO) {
                debug!("translating HTTP2 to orig-proto: {:?}", orig_proto);

                let val: &[u8] = orig_proto.as_bytes();

                if val.starts_with(b"HTTP/1.1") {
                    *req.version_mut() = http::Version::HTTP_11;
                } else if val.starts_with(b"HTTP/1.0") {
                    *req.version_mut() = http::Version::HTTP_10;
                } else {
                    warn!("unknown {} header value: {:?}", L5D_ORIG_PROTO, orig_proto,);
                }

                if was_absolute_form(val) {
                    req.extensions_mut().insert(h1::WasAbsoluteForm(()));
                }
                upgrade_response = true;
            }
        }

        let fut = self.inner.call(req);

        if upgrade_response {
            fut.map_ok(|mut res| {
                let orig_proto = match res.version() {
                    http::Version::HTTP_11 => "HTTP/1.1",
                    http::Version::HTTP_10 => "HTTP/1.0",
                    _ => return res,
                };

                res.headers_mut()
                    .insert(L5D_ORIG_PROTO, HeaderValue::from_static(orig_proto));

                // transfer-encoding is illegal in HTTP2
                res.headers_mut().remove(TRANSFER_ENCODING);

                *res.version_mut() = http::Version::HTTP_2;
                res
            })
        } else {
            fut.map_ok(|res| res)
        }
    }
}

fn was_absolute_form(val: &[u8]) -> bool {
    val.len() >= "HTTP/1.1; absolute-form".len() && &val[10..23] == b"absolute-form"
}
