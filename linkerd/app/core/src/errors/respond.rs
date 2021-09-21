use crate::svc;
use http::header::HeaderValue;
use linkerd_error::{Error, Result};
use linkerd_error_respond as respond;
pub use linkerd_proxy_http::{ClientHandle, HasH2Reason};
use linkerd_stack::ExtractParam;
use pin_project::pin_project;
use std::{
    borrow::Cow,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, info_span, warn};

pub const L5D_PROXY_ERROR: &str = "l5d-proxy-error";

pub fn layer<R, P: Clone, N>(
    params: P,
) -> impl svc::layer::Layer<N, Service = NewRespondService<R, P, N>> + Clone {
    respond::NewRespondService::layer(ExtractRespond(params))
}

pub type NewRespondService<R, P, N> =
    respond::NewRespondService<NewRespond<R>, ExtractRespond<P>, N>;

/// A strategy for responding to errors.
pub trait HttpRescue<E> {
    /// Attempts to synthesize a response from the given error.
    fn rescue(&self, error: E) -> Result<SyntheticHttpResponse, E>;
}

#[derive(Clone, Debug)]
pub struct SyntheticHttpResponse {
    pub grpc_status: tonic::Code,
    pub http_status: http::StatusCode,
    pub close_connection: bool,
    pub message: Cow<'static, str>,
}

#[derive(Clone, Debug)]
pub struct ExtractRespond<P>(P);

#[derive(Copy, Clone, Debug)]
pub struct NewRespond<R> {
    rescue: R,
}

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
    GrpcRescue {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
        rescue: R,
    },
}

const GRPC_CONTENT_TYPE: &str = "application/grpc";
const GRPC_STATUS: &str = "grpc-status";
const GRPC_MESSAGE: &str = "grpc-message";

// === impl HttpRescue ===

impl<E, F> HttpRescue<E> for F
where
    F: Fn(E) -> Result<SyntheticHttpResponse, E>,
{
    fn rescue(&self, error: E) -> Result<SyntheticHttpResponse, E> {
        (self)(error)
    }
}

// === impl SyntheticHttpResponse ===

impl SyntheticHttpResponse {
    pub fn unexpected_error() -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::INTERNAL_SERVER_ERROR,
            grpc_status: tonic::Code::Internal,
            message: Cow::Borrowed("unexpected error"),
        }
    }

    pub fn bad_gateway(msg: impl ToString) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::BAD_GATEWAY,
            grpc_status: tonic::Code::Unavailable,
            message: Cow::Owned(msg.to_string()),
        }
    }

    pub fn gateway_timeout(msg: impl ToString) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::GATEWAY_TIMEOUT,
            grpc_status: tonic::Code::Unavailable,
            message: Cow::Owned(msg.to_string()),
        }
    }

    pub fn unauthenticated(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::FORBIDDEN,
            grpc_status: tonic::Code::Unauthenticated,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
        }
    }

    pub fn permission_denied(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::FORBIDDEN,
            grpc_status: tonic::Code::PermissionDenied,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
        }
    }

    pub fn loop_detected(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::LOOP_DETECTED,
            grpc_status: tonic::Code::Aborted,
            close_connection: true,
            message: Cow::Owned(msg.to_string()),
        }
    }

    pub fn not_found(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::NOT_FOUND,
            grpc_status: tonic::Code::NotFound,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
        }
    }

    #[inline]
    fn message(&self) -> HeaderValue {
        match self.message {
            Cow::Borrowed(msg) => HeaderValue::from_static(msg),
            Cow::Owned(ref msg) => HeaderValue::from_str(&*msg).unwrap_or_else(|error| {
                warn!(%error, "Failed to encode error header");
                HeaderValue::from_static("unexpected error")
            }),
        }
    }

    #[inline]
    fn grpc_response<B: Default>(&self) -> http::Response<B> {
        debug!(code = %self.grpc_status, "Handling error on gRPC connection");
        http::Response::builder()
            .version(http::Version::HTTP_2)
            .header(http::header::CONTENT_LENGTH, "0")
            .header(http::header::CONTENT_TYPE, GRPC_CONTENT_TYPE)
            .header(GRPC_STATUS, code_header(self.grpc_status))
            .header(GRPC_MESSAGE, self.message())
            .header(L5D_PROXY_ERROR, self.message())
            .body(B::default())
            .expect("error response must be valid")
    }

    #[inline]
    fn http_response<B: Default>(&self, version: http::Version) -> http::Response<B> {
        debug!(status = %self.http_status, ?version, close = %self.close_connection, "Handling error on HTTP connection");
        let mut rsp = http::Response::builder()
            .status(self.http_status)
            .version(version)
            .header(http::header::CONTENT_LENGTH, "0")
            .header(L5D_PROXY_ERROR, self.message());

        if self.close_connection && version == http::Version::HTTP_11 {
            rsp = rsp.header(http::header::CONNECTION, "close");
        }

        rsp.body(B::default())
            .expect("error response must be valid")
    }
}

// === impl ExtractRespond ===

impl<T, R, P> ExtractParam<NewRespond<R>, T> for ExtractRespond<P>
where
    P: ExtractParam<R, T>,
{
    #[inline]
    fn extract_param(&self, t: &T) -> NewRespond<R> {
        NewRespond {
            rescue: self.0.extract_param(t),
        }
    }
}

// === impl NewRespond ===

impl<B, R> respond::NewRespond<http::Request<B>> for NewRespond<R>
where
    R: Clone,
{
    type Respond = Respond<R>;

    fn new_respond(&self, req: &http::Request<B>) -> Self::Respond {
        let client = req.extensions().get::<ClientHandle>().cloned();
        debug_assert!(client.is_some(), "Missing client handle");

        let rescue = self.rescue.clone();

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

impl<R> Respond<R> {
    fn client_addr(&self) -> std::net::SocketAddr {
        self.client
            .as_ref()
            .map(|ClientHandle { addr, .. }| *addr)
            .unwrap_or_else(|| {
                tracing::debug!("Missing client address");
                ([0, 0, 0, 0], 0).into()
            })
    }
}

impl<B, R> respond::Respond<http::Response<B>, Error> for Respond<R>
where
    B: Default + hyper::body::HttpBody,
    R: HttpRescue<Error> + Clone,
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
                    } => ResponseBody::GrpcRescue {
                        inner: b,
                        trailers: None,
                        rescue: rescue.clone(),
                    },
                    _ => ResponseBody::Passthru(b),
                }));
            }
            Err(error) => error,
        };

        let rsp = info_span!("rescue", client.addr = %self.client_addr()).in_scope(|| {
            tracing::info!(%error, "Request failed");
            self.rescue.rescue(error)
        })?;

        if rsp.close_connection {
            if let Some(ClientHandle { close, .. }) = self.client.as_ref() {
                close.close();
            } else {
                tracing::debug!("Missing client handle");
            }
        }

        let rsp = if self.is_grpc {
            rsp.grpc_response()
        } else {
            rsp.http_response(self.version)
        };

        Ok(rsp)
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
    fn grpc_trailers(code: tonic::Code, message: &str) -> http::HeaderMap {
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
fn code_header(code: tonic::Code) -> HeaderValue {
    use tonic::Code;
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
