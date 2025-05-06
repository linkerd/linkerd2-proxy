use super::{h1, h2, Body};
use futures::prelude::*;
use http::header::{HeaderValue, TRANSFER_ENCODING};
use http_body::Frame;
use linkerd_error::Result;
use linkerd_http_box::BoxBody;
use linkerd_stack::{layer, MakeConnection, Service};
use std::{
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
pub struct UpgradeResponseBody<B> {
    #[pin]
    inner: B,
}

/// Downgrades HTTP2 requests that were previousl upgraded to their original
/// protocol.
#[derive(Clone, Debug)]
pub struct Downgrade<S> {
    inner: S,
}

/// Extension that indicates a request was an orig-proto upgrade.
#[derive(Clone, Debug)]
pub struct WasUpgrade(());

/// An error returned by the [`Upgrade`] client.
///
/// This can represent an error presented by either of the underlying HTTP/1 or HTTP/2 clients,
/// or a "downgraded" HTTP/2 error.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Downgraded(#[from] DowngradedH2Error),
    #[error(transparent)]
    H1(linkerd_error::Error),
    #[error(transparent)]
    H2(hyper::Error),
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
    C: MakeConnection<(crate::Variant, T)> + Clone + Send + Sync + 'static,
    C::Connection: Unpin + Send,
    C::Future: Unpin + Send + 'static,
    B: crate::Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<linkerd_error::Error> + Send + Sync,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = Pin<
        Box<dyn Future<Output = Result<http::Response<BoxBody>, Self::Error>> + Send + 'static>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let Self { http1, h2 } = self;

        match http1.poll_ready(cx).map_err(Error::H1) {
            Poll::Ready(Ok(())) => {}
            poll => return poll,
        }

        h2.poll_ready(cx).map_err(Error::h2)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        debug_assert!(req.version() != http::Version::HTTP_2);
        if req
            .extensions()
            .get::<linkerd_http_upgrade::upgrade::Http11Upgrade>()
            .is_some()
        {
            debug!("Skipping orig-proto upgrade due to HTTP/1.1 upgrade");
            return Box::pin(
                self.http1
                    .call(req)
                    .map_ok(|rsp| rsp.map(BoxBody::new))
                    .map_err(Error::H1),
            );
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

        Box::pin(self.h2.call(req).map_err(Error::h2).map_ok(move |mut rsp| {
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
        }))
    }
}

// === impl Error ===

impl Error {
    fn h2(err: hyper::Error) -> Self {
        if let Some(downgraded) = downgrade_h2_error(&err) {
            return Self::Downgraded(downgraded);
        }

        Self::H2(err)
    }
}

/// Handles HTTP/2 client errors for HTTP/1.1 requests by wrapping the error type. This
/// simplifies error handling elsewhere so that HTTP/2 errors can only be encountered when the
/// original request was HTTP/2.
fn downgrade_h2_error<E: std::error::Error + Send + Sync + 'static>(
    orig: &E,
) -> Option<DowngradedH2Error> {
    #[inline]
    fn reason(e: &(dyn std::error::Error + 'static)) -> Option<h2::Reason> {
        e.downcast_ref::<h2::H2Error>()?.reason()
    }

    // If the provided error was an H2 error, wrap it as a downgraded error.
    if let Some(reason) = reason(orig) {
        return Some(DowngradedH2Error(reason));
    }

    // Otherwise, check the source chain to see if its original error was an H2 error.
    let mut cause = orig.source();
    while let Some(error) = cause {
        if let Some(reason) = reason(error) {
            return Some(DowngradedH2Error(reason));
        }

        cause = error.source();
    }

    // If the error was not an H2 error, return None.
    None
}

#[cfg(test)]
#[test]
fn test_downgrade_h2_error() {
    assert!(
        downgrade_h2_error(&h2::H2Error::from(h2::Reason::PROTOCOL_ERROR)).is_some(),
        "h2 errors must be downgraded"
    );

    #[derive(Debug, Error)]
    #[error("wrapped h2 error: {0}")]
    struct WrapError(#[source] linkerd_error::Error);
    assert!(
        downgrade_h2_error(&WrapError(
            h2::H2Error::from(h2::Reason::PROTOCOL_ERROR).into()
        ))
        .is_some(),
        "wrapped h2 errors must be downgraded"
    );

    assert!(
        downgrade_h2_error(&WrapError(
            WrapError(h2::H2Error::from(h2::Reason::PROTOCOL_ERROR).into()).into()
        ))
        .is_some(),
        "double-wrapped h2 errors must be downgraded"
    );

    assert!(
        downgrade_h2_error(&std::io::Error::new(
            std::io::ErrorKind::Other,
            "non h2 error"
        ))
        .is_none(),
        "other h2 errors must not be downgraded"
    );
}

#[cfg(test)]
#[test]
fn test_downgrade_error_source() {
    let err = Error::Downgraded(DowngradedH2Error(h2::Reason::PROTOCOL_ERROR));
    assert!(linkerd_error::is_caused_by::<DowngradedH2Error>(&err));
}

// === impl UpgradeResponseBody ===

impl<B> Body for UpgradeResponseBody<B>
where
    B: Body + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    type Data = B::Data;
    type Error = linkerd_error::Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx).map_err(|err| {
            downgrade_h2_error(&err)
                .map(Into::into)
                .unwrap_or_else(|| err.into())
        })
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        Body::size_hint(&self.inner)
    }
}

// === impl Downgrade ===

impl<S> Downgrade<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Copy {
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
                req.extensions_mut().insert(WasUpgrade(()));
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
