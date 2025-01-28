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
pub enum ResponseBody<R, B> {
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

impl<R, B: Default + linkerd_proxy_http::Body> Default for ResponseBody<R, B> {
    fn default() -> Self {
        ResponseBody::Passthru(B::default())
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
        match self.project() {
            ResponseBodyProj::Passthru(inner) => inner.poll_data(cx),
            ResponseBodyProj::GrpcRescue {
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
        match self.project() {
            ResponseBodyProj::Passthru(inner) => inner.poll_trailers(cx),
            ResponseBodyProj::GrpcRescue {
                inner, trailers, ..
            } => match trailers.take() {
                Some(t) => Poll::Ready(Ok(Some(t))),
                None => inner.poll_trailers(cx),
            },
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        match self {
            Self::Passthru(inner) => inner.is_end_stream(),
            Self::GrpcRescue {
                inner, trailers, ..
            } => trailers.is_none() && inner.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            Self::Passthru(inner) => inner.size_hint(),
            Self::GrpcRescue { inner, .. } => inner.size_hint(),
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
