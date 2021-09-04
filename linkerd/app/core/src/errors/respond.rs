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
use tracing::{debug, warn};

pub const L5D_PROXY_ERROR: &str = "l5d-proxy-error";

pub fn layer() -> respond::RespondLayer<NewRespond<super::DefaultRescue>> {
    respond::RespondLayer::new(NewRespond(super::DefaultRescue))
}

pub trait Rescue {
    fn rescue(
        &self,
        version: http::Version,
        error: Error,
        client: Option<&ClientHandle>,
    ) -> Result<Rescued>;
}

#[derive(Clone, Debug)]
pub struct Rescued {
    pub grpc_status: grpc::Code,
    pub http_status: http::StatusCode,
    pub message: String,
}

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
    NonGrpc(#[pin] B),
    Grpc {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
        rescue: R,
    },
}

const GRPC_CONTENT_TYPE: &str = "application/grpc";
const GRPC_STATUS: &str = "grpc-status";
const GRPC_MESSAGE: &str = "grpc-message";

// === impl NewRespond ===

impl<B, R> respond::NewRespond<http::Request<B>> for NewRespond<R>
where
    R: Clone + Rescue,
{
    type Respond = Respond<R>;

    fn new_respond(&self, req: &http::Request<B>) -> Self::Respond {
        let client = req.extensions().get::<ClientHandle>().cloned();
        debug_assert!(client.is_some(), "Missing client handle");

        match req.version() {
            http::Version::HTTP_2 => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok().map(|s| s.starts_with(GRPC_CONTENT_TYPE)))
                    .unwrap_or(false);
                Respond {
                    is_grpc,
                    client,
                    version: http::Version::HTTP_2,
                    rescue: self.0.clone(),
                }
            }
            version => Respond {
                version,
                client,
                is_grpc: false,
                rescue: self.0.clone(),
            },
        }
    }
}

// === impl Respond ===

impl<B, R> respond::Respond<http::Response<B>, Error> for Respond<R>
where
    B: Default + hyper::body::HttpBody,
    R: Rescue + Clone,
{
    type Response = http::Response<ResponseBody<R, B>>;

    fn respond(&self, res: Result<http::Response<B>>) -> Result<Self::Response> {
        let error = match res {
            Ok(rsp) => {
                return Ok(rsp.map(|b| match *self {
                    Respond { is_grpc: true, .. } => ResponseBody::Grpc {
                        inner: b,
                        trailers: None,
                        rescue: self.rescue.clone(),
                    },
                    _ => ResponseBody::NonGrpc(b),
                }))
            }
            Err(error) => error,
        };

        let Rescued {
            grpc_status,
            http_status,
            message,
        } = self
            .rescue
            .rescue(self.version, error, self.client.as_ref())?;
        let rsp = if self.is_grpc {
            Self::grpc_response(grpc_status, &*message)
        } else {
            Self::http_response(self.version, http_status, &*message)
        };

        Ok(rsp)
    }
}

impl<R> Respond<R> {
    fn http_response<B: Default>(
        version: http::Version,
        status: http::StatusCode,
        message: &str,
    ) -> http::Response<B> {
        debug!(%status, ?version, "http");
        http::Response::builder()
            .status(status)
            .version(version)
            .header(http::header::CONTENT_LENGTH, "0")
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

    fn grpc_response<B: Default>(code: grpc::Code, message: &str) -> http::Response<B> {
        debug!(%code, "grpc");

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
        ResponseBody::NonGrpc(B::default())
    }
}

impl<R, B> hyper::body::HttpBody for ResponseBody<R, B>
where
    B: hyper::body::HttpBody<Error = Error>,
    R: Rescue,
{
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.project() {
            ResponseBodyProj::NonGrpc(inner) => inner.poll_data(cx),
            ResponseBodyProj::Grpc {
                inner,
                trailers,
                rescue,
            } => {
                // should not be calling poll_data if we have set trailers derived from an error
                assert!(trailers.is_none());
                match inner.poll_data(cx) {
                    Poll::Ready(Some(Err(error))) => {
                        let Rescued {
                            grpc_status,
                            http_status: _,
                            message,
                        } = rescue.rescue(http::Version::HTTP_2, error, None)?;
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
            ResponseBodyProj::NonGrpc(inner) => inner.poll_trailers(cx),
            ResponseBodyProj::Grpc {
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
            Self::NonGrpc(inner) => inner.is_end_stream(),
            Self::Grpc {
                inner, trailers, ..
            } => trailers.is_none() && inner.is_end_stream(),
        }
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            Self::NonGrpc(inner) => inner.size_hint(),
            Self::Grpc { inner, .. } => inner.size_hint(),
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
