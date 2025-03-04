use super::{
    header::{GRPC_MESSAGE, GRPC_STATUS},
    respond::{HttpRescue, SyntheticHttpResponse},
};
use http::{header::HeaderValue, HeaderMap};
use http_body::Frame;
use linkerd_error::{Error, Result};
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, warn};

/// Returns a "gRPC rescue" body.
///
/// This returns a body that, should the inner `B`-typed body return an error when polling for
/// DATA frames, will "rescue" the stream and return a TRAILERS frame that describes the error.
#[pin_project(project = ResponseBodyProj)]
pub struct ResponseBody<R, B>(#[pin] Inner<R, B>);

#[pin_project(project = InnerProj)]
enum Inner<R, B> {
    /// An inert body that delegates directly down to the underlying body `B`.
    Passthru(#[pin] B),
    /// A body that will be rescued if it yields an error.
    GrpcRescue {
        #[pin]
        inner: B,
        /// An error response [strategy][HttpRescue].
        rescue: R,
        emit_headers: bool,
    },
    /// The underlying body `B` yielded an error and was "rescued".
    Rescued { trailers: Option<http::HeaderMap> },
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

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let ResponseBodyProj(inner) = self.as_mut().project();
        match inner.project() {
            InnerProj::Passthru(inner) => inner.poll_frame(cx),
            InnerProj::GrpcRescue {
                inner,
                rescue,
                emit_headers,
            } => match inner.poll_frame(cx) {
                Poll::Ready(Some(Err(error))) => {
                    // The inner body has yielded an error, which we will try to rescue. If so,
                    // store our synthetic trailers reporting the error.
                    let trailers = Self::rescue(error, rescue, *emit_headers)?;
                    self.set_rescued(trailers);
                    Poll::Ready(None)
                }
                data => data,
            },
            InnerProj::Rescued { trailers } => {
                let trailers = trailers.take().map(Frame::trailers).map(Ok);
                Poll::Ready(trailers)
            }
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        let Self(inner) = self;
        match inner {
            Inner::Passthru(inner) => inner.is_end_stream(),
            Inner::GrpcRescue { inner, .. } => inner.is_end_stream(),
            Inner::Rescued { trailers } => trailers.is_none(),
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        let Self(inner) = self;
        match inner {
            Inner::Passthru(inner) => inner.size_hint(),
            Inner::GrpcRescue { inner, .. } => inner.size_hint(),
            Inner::Rescued { .. } => http_body::SizeHint::with_exact(0),
        }
    }
}

impl<R, B> ResponseBody<R, B>
where
    B: http_body::Body,
    R: HttpRescue<B::Error>,
{
    /// Maps an error yielded by the inner body to a collection of gRPC trailers.
    ///
    /// This function returns `Ok(trailers)` if the given [`HttpRescue<E>`] strategy could identify
    /// a cause for an error yielded by the inner `B`-typed body.
    fn rescue(
        error: B::Error,
        rescue: &R,
        emit_headers: bool,
    ) -> Result<http::HeaderMap, B::Error> {
        let SyntheticHttpResponse {
            grpc_status,
            message,
            ..
        } = rescue.rescue(error)?;

        debug!(grpc.status = ?grpc_status, "Synthesizing gRPC trailers");
        let mut t = http::HeaderMap::new();
        t.insert(GRPC_STATUS, super::code_header(grpc_status));
        if emit_headers {
            // A gRPC message trailer is only included if instructed to emit additional headers.
            t.insert(
                GRPC_MESSAGE,
                HeaderValue::from_str(&message).unwrap_or_else(|error| {
                    warn!(%error, "Failed to encode error header");
                    HeaderValue::from_static("Unexpected error")
                }),
            );
        }

        Ok(t)
    }
}

impl<R, B> ResponseBody<R, B> {
    /// Marks this body as "rescued".
    ///
    /// No more data frames will be yielded, and the given trailers will be returned when this
    /// body is polled.
    fn set_rescued(mut self: Pin<&mut Self>, trailers: HeaderMap) {
        let trailers = Some(trailers);
        let new = Self(Inner::Rescued { trailers });
        self.set(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::header::{GRPC_MESSAGE, GRPC_STATUS};
    use http::HeaderMap;
    use linkerd_mock_http_body::MockBody;

    struct MockRescue;
    impl<E> HttpRescue<E> for MockRescue {
        /// Attempts to synthesize a response from the given error.
        fn rescue(&self, _: E) -> Result<SyntheticHttpResponse, E> {
            let synthetic = SyntheticHttpResponse::internal_error("MockRescue::rescue");
            Ok(synthetic)
        }
    }

    #[tokio::test]
    async fn rescue_body_recovers_from_error_without_grpc_message() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let trailers = {
            let mut trls = HeaderMap::with_capacity(1);
            let value = HeaderValue::from_static("caboose");
            trls.insert("trailer", value);
            trls
        };
        let rescue = {
            let inner = MockBody::default()
                .then_yield_data(Poll::Ready(Some(Ok("inter".into()))))
                .then_yield_data(Poll::Ready(Some(Err("an error midstream".into()))))
                .then_yield_data(Poll::Ready(Some(Ok("rupted".into()))))
                .then_yield_trailer(Poll::Ready(Some(Ok(trailers))));
            let rescue = MockRescue;
            let emit_headers = false;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, Some(trailers)) = body_to_string(rescue).await else {
            panic!("trailers should exist");
        };
        assert_eq!(data, "inter");
        assert_eq!(
            trailers[GRPC_STATUS],
            i32::from(tonic::Code::Internal).to_string()
        );
        assert_eq!(trailers.get(GRPC_MESSAGE), None);
    }

    #[tokio::test]
    async fn rescue_body_recovers_from_error_emitting_message() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let trailers = {
            let mut trls = HeaderMap::with_capacity(1);
            let value = HeaderValue::from_static("caboose");
            trls.insert("trailer", value);
            trls
        };
        let rescue = {
            let inner = MockBody::default()
                .then_yield_data(Poll::Ready(Some(Ok("inter".into()))))
                .then_yield_data(Poll::Ready(Some(Err("an error midstream".into()))))
                .then_yield_data(Poll::Ready(Some(Ok("rupted".into()))))
                .then_yield_trailer(Poll::Ready(Some(Ok(trailers))));
            let rescue = MockRescue;
            let emit_headers = true;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, Some(trailers)) = body_to_string(rescue).await else {
            panic!("trailers should exist");
        };
        assert_eq!(data, "inter");
        assert_eq!(
            trailers[GRPC_STATUS],
            i32::from(tonic::Code::Internal).to_string()
        );
        assert_eq!(trailers[GRPC_MESSAGE], "MockRescue::rescue");
    }

    #[tokio::test]
    async fn rescue_body_works_for_empty() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let rescue = {
            let inner = MockBody::default();
            let rescue = MockRescue;
            let emit_headers = false;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, trailers) = body_to_string(rescue).await;
        assert_eq!(data, "");
        assert_eq!(trailers, None);
    }

    #[tokio::test]
    async fn rescue_body_works_for_body_with_data() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let rescue = {
            let inner = MockBody::default().then_yield_data(Poll::Ready(Some(Ok("unary".into()))));
            let rescue = MockRescue;
            let emit_headers = false;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, trailers) = body_to_string(rescue).await;
        assert_eq!(data, "unary");
        assert_eq!(trailers, None);
    }

    #[tokio::test]
    async fn rescue_body_works_for_body_with_trailers() {
        let (_guard, _handle) = linkerd_tracing::test::trace_init();
        let trailers = {
            let mut trls = HeaderMap::with_capacity(1);
            let value = HeaderValue::from_static("caboose");
            trls.insert("trailer", value);
            trls
        };
        let rescue = {
            let inner = MockBody::default().then_yield_trailer(Poll::Ready(Some(Ok(trailers))));
            let rescue = MockRescue;
            let emit_headers = false;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, trailers) = body_to_string(rescue).await;
        assert_eq!(data, "");
        assert_eq!(trailers.expect("has trailers")["trailer"], "caboose");
    }

    async fn body_to_string<B>(mut body: B) -> (String, Option<HeaderMap>)
    where
        B: http_body::Body + Unpin,
        B::Error: std::fmt::Debug,
    {
        use http_body_util::BodyExt;

        let mut data = String::new();
        let mut trailers = None;

        // Continue reading frames from the body until it is finished.
        while let Some(frame) = body
            .frame()
            .await
            .transpose()
            .expect("reading a frame succeeds")
        {
            match frame.into_data().map(|mut buf| {
                use bytes::Buf;
                let bytes = buf.copy_to_bytes(buf.remaining());
                String::from_utf8(bytes.to_vec()).unwrap()
            }) {
                Ok(ref s) => data.push_str(s),
                Err(frame) => {
                    let trls = frame
                        .into_trailers()
                        .map_err(drop)
                        .expect("test frame is either data or trailers");
                    trailers = Some(trls);
                }
            }
        }

        tracing::info!(?data, ?trailers, "finished reading body");
        (data, trailers)
    }
}
