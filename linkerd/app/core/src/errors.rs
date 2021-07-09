use http::{header::HeaderValue, StatusCode};
use linkerd_errno::Errno;
use linkerd_error::Error;
use linkerd_error_metrics::{self as error_metrics, RecordErrorLayer, Registry};
use linkerd_error_respond as respond;
pub use linkerd_error_respond::RespondLayer;
use linkerd_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd_proxy_http::{ClientHandle, HasH2Reason};
use linkerd_timeout::{FailFastError, ResponseTimeout};
use linkerd_tls as tls;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;
use tonic::{self as grpc, Code};
use tracing::{debug, warn};

metrics! {
    inbound_http_errors_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    },

    outbound_http_errors_total: Counter {
        "The total number of outbound HTTP requests that could not be processed due to a proxy error."
    }
}

pub fn layer() -> respond::RespondLayer<NewRespond> {
    respond::RespondLayer::new(NewRespond(()))
}

#[derive(Clone)]
pub struct Metrics {
    inbound: Registry<Reason>,
    outbound: Registry<Reason>,
}

pub type MetricsLayer = RecordErrorLayer<LabelError, Reason>;

/// Error metric labels.
#[derive(Copy, Clone, Debug)]
pub struct LabelError(());

#[derive(Copy, Clone, Debug, Error)]
#[error("{}", self.message)]
pub struct HttpError {
    http: http::StatusCode,
    grpc: Code,
    message: &'static str,
    reason: Reason,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    DispatchTimeout,
    ResponseTimeout,
    IdentityRequired,
    Io(Option<Errno>),
    FailFast,
    GatewayLoop,
    NotFound,
    Unexpected,
}

#[derive(Copy, Clone, Debug)]
pub struct NewRespond(());

#[derive(Clone, Debug)]
pub struct Respond {
    version: http::Version,
    is_grpc: bool,
    client: Option<ClientHandle>,
}

#[pin_project(project = ResponseBodyProj)]
pub enum ResponseBody<B> {
    NonGrpc(#[pin] B),
    Grpc {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
    },
}

const GRPC_CONTENT_TYPE: &str = "application/grpc";

impl<B: hyper::body::HttpBody> hyper::body::HttpBody for ResponseBody<B>
where
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.project() {
            ResponseBodyProj::NonGrpc(inner) => inner.poll_data(cx),
            ResponseBodyProj::Grpc { inner, trailers } => {
                // should not be calling poll_data if we have set trailers derived from an error
                assert!(trailers.is_none());
                match inner.poll_data(cx) {
                    Poll::Ready(Some(Err(error))) => {
                        let error = error.into();
                        let mut error_trailers = http::HeaderMap::new();
                        let code = set_grpc_status(&*error, &mut error_trailers);
                        debug!(%error, grpc.status = ?code, "Handling gRPC stream failure");
                        *trailers = Some(error_trailers);
                        Poll::Ready(None)
                    }
                    data => data,
                }
            }
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        match self.project() {
            ResponseBodyProj::NonGrpc(inner) => inner.poll_trailers(cx),
            ResponseBodyProj::Grpc { inner, trailers } => match trailers.take() {
                Some(t) => Poll::Ready(Ok(Some(t))),
                None => inner.poll_trailers(cx),
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::NonGrpc(inner) => inner.is_end_stream(),
            Self::Grpc { inner, trailers } => trailers.is_none() && inner.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            Self::NonGrpc(inner) => inner.size_hint(),
            Self::Grpc { inner, .. } => inner.size_hint(),
        }
    }
}

impl<B: Default + hyper::body::HttpBody> Default for ResponseBody<B> {
    fn default() -> ResponseBody<B> {
        ResponseBody::NonGrpc(B::default())
    }
}

impl<ReqB, RspB: Default + hyper::body::HttpBody>
    respond::NewRespond<http::Request<ReqB>, http::Response<RspB>> for NewRespond
{
    type Response = http::Response<ResponseBody<RspB>>;
    type Respond = Respond;

    fn new_respond(&self, req: &http::Request<ReqB>) -> Self::Respond {
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
                }
            }
            version => Respond {
                version,
                client,
                is_grpc: false,
            },
        }
    }
}

impl<RspB: Default + hyper::body::HttpBody> respond::Respond<http::Response<RspB>> for Respond {
    type Response = http::Response<ResponseBody<RspB>>;

    fn respond(&self, res: Result<http::Response<RspB>, Error>) -> Result<Self::Response, Error> {
        match res {
            Ok(response) => Ok(response.map(|b| match *self {
                Respond { is_grpc: true, .. } => ResponseBody::Grpc {
                    inner: b,
                    trailers: None,
                },
                _ => ResponseBody::NonGrpc(b),
            })),
            Err(error) => {
                let addr = self
                    .client
                    .as_ref()
                    .map(|ClientHandle { ref addr, .. }| *addr)
                    .unwrap_or_else(|| {
                        debug!("Missing client address");
                        ([0, 0, 0, 0], 0).into()
                    });
                warn!(client.addr = %addr, "Failed to proxy request: {}", error);

                if self.version == http::Version::HTTP_2 {
                    if let Some(reset) = error.h2_reason() {
                        debug!(%reset, "Propagating HTTP2 reset");
                        return Err(error);
                    }
                }

                // Gracefully teardown the server-side connection.
                if let Some(ClientHandle { ref close, .. }) = self.client.as_ref() {
                    debug!("Closing server-side connection");
                    close.close();
                }

                if self.is_grpc {
                    let mut rsp = http::Response::builder()
                        .version(http::Version::HTTP_2)
                        .header(http::header::CONTENT_LENGTH, "0")
                        .header(http::header::CONTENT_TYPE, GRPC_CONTENT_TYPE)
                        .body(ResponseBody::default())
                        .expect("app::errors response is valid");
                    let code = set_grpc_status(&*error, rsp.headers_mut());
                    debug!(?code, "Handling error with gRPC status");
                    return Ok(rsp);
                }

                let status = http_status(&*error);
                debug!(%status, version = ?self.version, "Handling error with HTTP response");
                Ok(http::Response::builder()
                    .version(self.version)
                    .status(status)
                    .header(http::header::CONTENT_LENGTH, "0")
                    .body(ResponseBody::default())
                    .expect("error response must be valid"))
            }
        }
    }
}

fn http_status(error: &(dyn std::error::Error + 'static)) -> StatusCode {
    if let Some(HttpError { http, .. }) = error.downcast_ref::<HttpError>() {
        *http
    } else if error.is::<ResponseTimeout>() {
        http::StatusCode::GATEWAY_TIMEOUT
    } else if error.is::<FailFastError>() || error.is::<tower::timeout::error::Elapsed>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<IdentityRequired>() {
        http::StatusCode::FORBIDDEN
    } else if let Some(source) = error.source() {
        http_status(source)
    } else {
        http::StatusCode::BAD_GATEWAY
    }
}

fn set_grpc_status(
    error: &(dyn std::error::Error + 'static),
    headers: &mut http::HeaderMap,
) -> grpc::Code {
    const GRPC_STATUS: &str = "grpc-status";
    const GRPC_MESSAGE: &str = "grpc-message";

    if let Some(HttpError { grpc, message, .. }) = error.downcast_ref::<HttpError>() {
        headers.insert(GRPC_STATUS, code_header(*grpc));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static(message));
        *grpc
    } else if error.is::<ResponseTimeout>() {
        let code = Code::DeadlineExceeded;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static("request timed out"));
        code
    } else if let Some(e) = error.downcast_ref::<FailFastError>() {
        let code = Code::Unavailable;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_str(&e.to_string()).unwrap_or_else(|error| {
                warn!(%error, "Failed to encode fail-fast error message");
                HeaderValue::from_static("Service in fail-fast")
            }),
        );
        code
    } else if error.is::<tower::timeout::error::Elapsed>() {
        let code = Code::Unavailable;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_static("proxy dispatch timed out"),
        );
        code
    } else if error.is::<IdentityRequired>() {
        let code = Code::FailedPrecondition;
        headers.insert(GRPC_STATUS, code_header(code));
        if let Ok(msg) = HeaderValue::from_str(&error.to_string()) {
            headers.insert(GRPC_MESSAGE, msg);
        }
        code
    } else if error.is::<std::io::Error>() {
        let code = Code::Unavailable;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static("connection closed"));
        code
    } else if let Some(source) = error.source() {
        set_grpc_status(source, headers)
    } else {
        let code = Code::Internal;
        headers.insert(GRPC_STATUS, code_header(code));
        if let Ok(msg) = HeaderValue::from_str(&error.to_string()) {
            headers.insert(GRPC_MESSAGE, msg);
        }
        code
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
        Code::__NonExhaustive => unreachable!("Code::__NonExhaustive"),
    }
}

#[derive(Debug)]
pub struct IdentityRequired {
    pub required: tls::client::ServerId,
    pub found: Option<tls::client::ServerId>,
}

impl std::fmt::Display for IdentityRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.found {
            Some(ref found) => write!(
                f,
                "request required the identity '{}' but '{}' found",
                self.required, found
            ),
            None => write!(
                f,
                "request required the identity '{}' but no identity found",
                self.required
            ),
        }
    }
}

impl std::error::Error for IdentityRequired {}

impl LabelError {
    fn reason(err: &(dyn std::error::Error + 'static)) -> Reason {
        if let Some(HttpError { reason, .. }) = err.downcast_ref::<HttpError>() {
            *reason
        } else if err.is::<ResponseTimeout>() {
            Reason::ResponseTimeout
        } else if err.is::<FailFastError>() {
            Reason::FailFast
        } else if err.is::<tower::timeout::error::Elapsed>() {
            Reason::DispatchTimeout
        } else if err.is::<IdentityRequired>() {
            Reason::IdentityRequired
        } else if let Some(e) = err.downcast_ref::<std::io::Error>() {
            Reason::Io(e.raw_os_error().map(Errno::from))
        } else if let Some(e) = err.source() {
            Self::reason(e)
        } else {
            Reason::Unexpected
        }
    }
}

impl error_metrics::LabelError<Error> for LabelError {
    type Labels = Reason;

    fn label_error(&self, err: &Error) -> Self::Labels {
        Self::reason(err.as_ref())
    }
}

impl FmtLabels for Reason {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "message=\"{}\"",
            match self {
                Reason::FailFast => "failfast",
                Reason::DispatchTimeout => "dispatch timeout",
                Reason::ResponseTimeout => "response timeout",
                Reason::IdentityRequired => "identity required",
                Reason::GatewayLoop => "gateway loop",
                Reason::NotFound => "not found",
                Reason::Io(_) => "i/o",
                Reason::Unexpected => "unexpected",
            }
        )?;

        if let Reason::Io(Some(errno)) = self {
            write!(f, ",errno=\"{}\"", errno)?;
        }

        Ok(())
    }
}

impl Default for Metrics {
    fn default() -> Metrics {
        Self {
            inbound: Registry::new(inbound_http_errors_total),
            outbound: Registry::new(outbound_http_errors_total),
        }
    }
}

impl Metrics {
    pub fn inbound(&self) -> MetricsLayer {
        self.inbound.layer(LabelError(()))
    }

    pub fn outbound(&self) -> MetricsLayer {
        self.outbound.layer(LabelError(()))
    }

    pub fn report(&self) -> impl FmtMetrics + Clone + Send {
        self.inbound.clone().and_then(self.outbound.clone())
    }
}

impl HttpError {
    pub fn identity_required(message: &'static str) -> Self {
        Self {
            message,
            http: http::StatusCode::FORBIDDEN,
            grpc: Code::Unauthenticated,
            reason: Reason::IdentityRequired,
        }
    }

    pub fn not_found(message: &'static str) -> Self {
        Self {
            message,
            http: http::StatusCode::NOT_FOUND,
            grpc: Code::NotFound,
            reason: Reason::NotFound,
        }
    }

    pub fn gateway_loop() -> Self {
        Self {
            message: "gateway loop detected",
            http: http::StatusCode::LOOP_DETECTED,
            grpc: Code::Aborted,
            reason: Reason::GatewayLoop,
        }
    }

    pub fn status(&self) -> http::StatusCode {
        self.http
    }
}
