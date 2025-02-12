use super::{
    header::{GRPC_MESSAGE, GRPC_STATUS},
    respond::{HttpRescue, SyntheticHttpResponse},
};
use http::header::HeaderValue;
use linkerd_error::{Error, Result};
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, warn};

#[pin_project(project = ResponseBodyProj)]
pub struct ResponseBody<R, B>(#[pin] Inner<R, B>);

#[pin_project(project = InnerProj)]
enum Inner<R, B> {
    Passthru(#[pin] B),
    GrpcRescue {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
        rescue: R,
        emit_headers: bool,
    },
}

// === impl ResponseBody ===

impl<R, B> ResponseBody<R, B> {
    /// Returns a body in "passthru" mode.
    pub fn passthru(inner: B) -> Self {
        Self(Inner::Passthru(inner))
    }

    /// Returns a "gRPC rescue" body.
    pub fn grpc_rescue(inner: B, rescue: R, emit_headers: bool) -> Self {
        Self(Inner::GrpcRescue {
            inner,
            rescue,
            emit_headers,
            trailers: None,
        })
    }
}

impl<R, B: Default + linkerd_proxy_http::Body> Default for ResponseBody<R, B> {
    fn default() -> Self {
        Self(Inner::Passthru(B::default()))
    }
}

impl<R, B> linkerd_proxy_http::Body for ResponseBody<R, B>
where
    B: linkerd_proxy_http::Body<Error = Error>,
    R: HttpRescue<B::Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let ResponseBodyProj(inner) = self.project();
        match inner.project() {
            InnerProj::Passthru(inner) => inner.poll_data(cx),
            InnerProj::GrpcRescue {
                inner,
                trailers,
                rescue,
                emit_headers,
            } => {
                // should not be calling poll_data if we have set trailers derived from an error
                assert!(trailers.is_none());
                match inner.poll_data(cx) {
                    Poll::Ready(Some(Err(error))) => {
                        let SyntheticHttpResponse {
                            grpc_status,
                            message,
                            ..
                        } = rescue.rescue(error)?;
                        let t = Self::grpc_trailers(grpc_status, &message, *emit_headers);
                        *trailers = Some(t);
                        Poll::Ready(None)
                    }
                    data => data,
                }
            }
        }
    }

    #[inline]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let ResponseBodyProj(inner) = self.project();
        match inner.project() {
            InnerProj::Passthru(inner) => inner.poll_trailers(cx),
            InnerProj::GrpcRescue {
                inner, trailers, ..
            } => match trailers.take() {
                Some(t) => Poll::Ready(Ok(Some(t))),
                None => inner.poll_trailers(cx),
            },
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        let Self(inner) = self;
        match inner {
            Inner::Passthru(inner) => inner.is_end_stream(),
            Inner::GrpcRescue {
                inner, trailers, ..
            } => trailers.is_none() && inner.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        let Self(inner) = self;
        match inner {
            Inner::Passthru(inner) => inner.size_hint(),
            Inner::GrpcRescue { inner, .. } => inner.size_hint(),
        }
    }
}

impl<R, B> ResponseBody<R, B> {
    fn grpc_trailers(code: tonic::Code, message: &str, emit_headers: bool) -> http::HeaderMap {
        debug!(grpc.status = ?code, "Synthesizing gRPC trailers");
        let mut t = http::HeaderMap::new();
        t.insert(GRPC_STATUS, super::code_header(code));
        if emit_headers {
            t.insert(
                GRPC_MESSAGE,
                HeaderValue::from_str(message).unwrap_or_else(|error| {
                    warn!(%error, "Failed to encode error header");
                    HeaderValue::from_static("Unexpected error")
                }),
            );
        }
        t
    }
}
