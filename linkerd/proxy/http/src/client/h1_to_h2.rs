use super::{h1, h2};
use crate::header::L5D_ORIG_PROTO;
use futures::prelude::*;
use http::header::{HeaderValue, TRANSFER_ENCODING};
use hyper::body::HttpBody;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_stack::{MakeConnection, Service};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, trace};

/// Upgrades HTTP requests from their original protocol to HTTP2.
#[derive(Debug)]
pub struct Http1OverH2<C, T, B> {
    http1: h1::Client<C, T, B>,
    h2: h2::Connection<B>,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("upgraded connection failed with HTTP/2 reset: {0}")]
pub struct DowngradedH2Error(h2::Reason);

#[pin_project::pin_project]
#[derive(Debug, Default)]
struct ResponseBody {
    inner: hyper::Body,
}

// === impl Http1OverH2 ===

impl<C, T, B> Http1OverH2<C, T, B> {
    pub(crate) fn new(http1: h1::Client<C, T, B>, h2: h2::Connection<B>) -> Self {
        Self { http1, h2 }
    }
}

impl<C, T, B> Service<http::Request<B>> for Http1OverH2<C, T, B>
where
    T: Clone + Send + Sync + 'static,
    C: MakeConnection<(crate::Version, T)> + Clone + Send + Sync + 'static,
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
        if req
            .extensions()
            .get::<crate::upgrade::Http11Upgrade>()
            .is_some()
        {
            debug!("Skipping orig-proto upgrade due to HTTP/1.1 upgrade");
            return Box::pin(self.http1.request(req).map_ok(|rsp| rsp.map(BoxBody::new)));
        }

        let orig_version = req.version();
        let absolute_form = req
            .extensions_mut()
            .remove::<crate::server::UriWasOriginallyAbsoluteForm>()
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
                    rsp.map(|inner| BoxBody::new(ResponseBody { inner }))
                }),
        )
    }
}

// === impl UpgradeResponseBody ===

impl HttpBody for ResponseBody {
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

    #[derive(Debug, thiserror::Error)]
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
