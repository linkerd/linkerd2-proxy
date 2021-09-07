use http::header::HeaderValue;
use linkerd_error::{Error, Result};
use linkerd_error_respond as respond;
pub use linkerd_proxy_http::{ClientHandle, HasH2Reason};
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tonic::{self as grpc, Code};
use tracing::{debug, info, info_span, warn};

pub const L5D_PROXY_ERROR: &str = "l5d-proxy-error";

/// A strategy for responding to errors.
pub trait Rescue<E> {
    /// Attempts to synthesize a response from the given error.
    fn rescue(&self, error: E) -> Result<SyntheticResponse, E>;

    /// A helper for inspecting potentially nested errors.
    fn has_cause<C: std::error::Error + 'static>(
        error: &(dyn std::error::Error + 'static),
    ) -> bool {
        error.is::<C>() || error.source().map(Self::has_cause::<C>).unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
pub struct SyntheticResponse {
    pub grpc_status: grpc::Code,
    pub http_status: http::StatusCode,
    pub close_connection: bool,
    pub message: String,
}

pub type Layer<R> = respond::RespondLayer<NewRespond<R>>;

#[derive(Copy, Clone, Debug)]
pub struct NewRespond<R>(R);

#[derive(Clone, Debug)]
pub struct Respond<R> {
    rescue: R,
    version: http::Version,
    is_grpc: bool,
    client: Option<ClientHandle>,
}

#[pin_project(project = ResponseBodyProj)]
pub enum ResponseBody<R, B> {
    Passthru(#[pin] B),
    RescueGrpc {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
        rescue: R,
    },
}

const GRPC_CONTENT_TYPE: &str = "application/grpc";
const GRPC_STATUS: &str = "grpc-status";
const GRPC_MESSAGE: &str = "grpc-message";

// === impl SyntheticResponse ===

impl Default for SyntheticResponse {
    fn default() -> Self {
        Self {
            http_status: http::StatusCode::INTERNAL_SERVER_ERROR,
            grpc_status: Code::Internal,
            message: "unexpected error".to_string(),
            close_connection: true,
        }
    }
}

// === impl NewRespond ===

impl<R> NewRespond<R> {
    pub fn layer(rescue: R) -> Layer<R> {
        respond::RespondLayer::new(NewRespond(rescue))
    }
}

impl<B, R> respond::NewRespond<http::Request<B>> for NewRespond<R>
where
    R: Clone,
{
    type Respond = Respond<R>;

    fn new_respond(&self, req: &http::Request<B>) -> Self::Respond {
        let client = req.extensions().get::<ClientHandle>().cloned();
        debug_assert!(client.is_some(), "Missing client handle");

        let rescue = self.0.clone();

        match req.version() {
            http::Version::HTTP_2 => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok().map(|s| s.starts_with(GRPC_CONTENT_TYPE)))
                    .unwrap_or(false);
                Respond {
                    client,
                    rescue,
                    is_grpc,
                    version: http::Version::HTTP_2,
                }
            }
            version => Respond {
                client,
                rescue,
                version,
                is_grpc: false,
            },
        }
    }
}

// === impl Respond ===

impl<B, R> respond::Respond<http::Response<B>, Error> for Respond<R>
where
    B: Default + hyper::body::HttpBody,
    R: Rescue<Error> + Clone,
{
    type Response = http::Response<ResponseBody<R, B>>;

    fn respond(&self, res: Result<http::Response<B>>) -> Result<Self::Response> {
        let error = match res {
            Ok(rsp) => {
                return Ok(rsp.map(|b| match self {
                    Respond {
                        is_grpc: true,
                        rescue,
                        ..
                    } => ResponseBody::RescueGrpc {
                        inner: b,
                        trailers: None,
                        rescue: rescue.clone(),
                    },
                    _ => ResponseBody::Passthru(b),
                }));
            }
            Err(error) => error,
        };

        let span = info_span!(
            "rescue",
            client.addr = %self.client
                .as_ref()
                .map(|ClientHandle { addr, .. }| *addr)
                .unwrap_or_else(|| {
                    tracing::debug!("Missing client address");
                    ([0, 0, 0, 0], 0).into()
                })
        );

        let SyntheticResponse {
            grpc_status,
            http_status,
            close_connection,
            message,
        } = span.in_scope(|| {
            tracing::info!(%error, "Request failed");
            self.rescue.rescue(error)
        })?;

        if close_connection {
            if let Some(ClientHandle { close, .. }) = self.client.as_ref() {
                close.close();
            } else {
                tracing::debug!("Missing client handle");
            }
        }

        let rsp = if self.is_grpc {
            Self::grpc_response(grpc_status, &*message)
        } else {
            Self::http_response(self.version, http_status, &*message, close_connection)
        };

        Ok(rsp)
    }
}

impl<R> Respond<R> {
    fn http_response<B: Default>(
        version: http::Version,
        status: http::StatusCode,
        message: &str,
        close_connection: bool,
    ) -> http::Response<B> {
        info!(%status, ?version, %message, close_connection, "Handling error on HTTP connection");
        let mut rsp = http::Response::builder()
            .status(status)
            .version(version)
            .header(http::header::CONTENT_LENGTH, "0")
            .header(
                L5D_PROXY_ERROR,
                HeaderValue::from_str(message).unwrap_or_else(|error| {
                    warn!(%error, "Failed to encode error header");
                    HeaderValue::from_static("Unexpected error")
                }),
            );

        if close_connection && version == http::Version::HTTP_11 {
            rsp = rsp.header(http::header::CONNECTION, "close");
        }

        rsp.body(B::default())
            .expect("error response must be valid")
    }

    fn grpc_response<B: Default>(code: grpc::Code, message: &str) -> http::Response<B> {
        info!(%code, %message, "Handling error on gRPC connection");

        http::Response::builder()
            .version(http::Version::HTTP_2)
            .header(http::header::CONTENT_LENGTH, "0")
            .header(http::header::CONTENT_TYPE, GRPC_CONTENT_TYPE)
            .header(GRPC_STATUS, code_header(code))
            .header(
                GRPC_MESSAGE,
                HeaderValue::from_str("request timed out").unwrap_or_else(|error| {
                    warn!(%error, "Failed to encode error header");
                    HeaderValue::from_static("Unexpected error")
                }),
            )
            .header(
                L5D_PROXY_ERROR,
                HeaderValue::from_str(message).unwrap_or_else(|error| {
                    warn!(%error, "Failed to encode error header");
                    HeaderValue::from_static("Unexpected error")
                }),
            )
            .body(B::default())
            .expect("error response must be valid")
    }
}

// === impl ResponseBody ===

impl<R, B: Default + hyper::body::HttpBody> Default for ResponseBody<R, B> {
    fn default() -> Self {
        ResponseBody::Passthru(B::default())
    }
}

impl<R, B> hyper::body::HttpBody for ResponseBody<R, B>
where
    B: hyper::body::HttpBody<Error = Error>,
    R: Rescue<B::Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.project() {
            ResponseBodyProj::Passthru(inner) => inner.poll_data(cx),
            ResponseBodyProj::RescueGrpc {
                inner,
                trailers,
                rescue,
            } => {
                // should not be calling poll_data if we have set trailers derived from an error
                assert!(trailers.is_none());
                match inner.poll_data(cx) {
                    Poll::Ready(Some(Err(error))) => {
                        let SyntheticResponse {
                            grpc_status,
                            message,
                            ..
                        } = rescue.rescue(error)?;
                        *trailers = Some(Self::grpc_trailers(grpc_status, &*message));
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
            ResponseBodyProj::RescueGrpc {
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
            Self::RescueGrpc {
                inner, trailers, ..
            } => trailers.is_none() && inner.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            Self::Passthru(inner) => inner.size_hint(),
            Self::RescueGrpc { inner, .. } => inner.size_hint(),
        }
    }
}

impl<R, B> ResponseBody<R, B> {
    fn grpc_trailers(code: grpc::Code, message: &str) -> http::HeaderMap {
        debug!(grpc.status = ?code, "Synthesizing gRPC trailers");
        let mut t = http::HeaderMap::new();
        t.insert(GRPC_STATUS, code_header(code));
        t.insert(
            GRPC_MESSAGE,
            HeaderValue::from_str(message).unwrap_or_else(|error| {
                warn!(%error, "Failed to encode error header");
                HeaderValue::from_static("Unexpected error")
            }),
        );
        t
    }
}

// Copied from tonic, where it's private.
fn code_header(code: grpc::Code) -> HeaderValue {
    match code {
        Code::Ok => HeaderValue::from_static("0"),
        Code::Cancelled => HeaderValue::from_static("1"),
        Code::Unknown => HeaderValue::from_static("2"),
        Code::InvalidArgument => HeaderValue::from_static("3"),
        Code::DeadlineExceeded => HeaderValue::from_static("4"),
        Code::NotFound => HeaderValue::from_static("5"),
        Code::AlreadyExists => HeaderValue::from_static("6"),
        Code::PermissionDenied => HeaderValue::from_static("7"),
        Code::ResourceExhausted => HeaderValue::from_static("8"),
        Code::FailedPrecondition => HeaderValue::from_static("9"),
        Code::Aborted => HeaderValue::from_static("10"),
        Code::OutOfRange => HeaderValue::from_static("11"),
        Code::Unimplemented => HeaderValue::from_static("12"),
        Code::Internal => HeaderValue::from_static("13"),
        Code::Unavailable => HeaderValue::from_static("14"),
        Code::DataLoss => HeaderValue::from_static("15"),
        Code::Unauthenticated => HeaderValue::from_static("16"),
    }
}
