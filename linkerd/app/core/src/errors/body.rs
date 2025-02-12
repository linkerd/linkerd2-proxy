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

/// Returns a "gRPC rescue" body.
///
/// This returns a body that, should the inner `B`-typed body return an error when polling for
/// DATA frames, will "rescue" the stream and return a TRAILERS frame that describes the error.
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
                .then_yield_trailer(Poll::Ready(Ok(Some(trailers))));
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
                .then_yield_trailer(Poll::Ready(Ok(Some(trailers))));
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
            let inner = MockBody::default().then_yield_trailer(Poll::Ready(Ok(Some(trailers))));
            let rescue = MockRescue;
            let emit_headers = false;
            ResponseBody::grpc_rescue(inner, rescue, emit_headers)
        };
        let (data, trailers) = body_to_string(rescue).await;
        assert_eq!(data, "");
        assert_eq!(trailers.expect("has trailers")["trailer"], "caboose");
    }

    async fn body_to_string<B>(body: B) -> (String, Option<HeaderMap>)
    where
        B: http_body::Body + Unpin,
        B::Error: std::fmt::Debug,
    {
        let mut body = linkerd_http_body_compat::ForwardCompatibleBody::new(body);
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
