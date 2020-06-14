use crate::proxy::identity;
use http::{header::HeaderValue, StatusCode};
use linkerd2_errno::Errno;
use linkerd2_error::Error;
use linkerd2_error_metrics as metrics;
use linkerd2_error_respond as respond;
pub use linkerd2_error_respond::RespondLayer;
use linkerd2_proxy_http::HasH2Reason;
use linkerd2_timeout::{error::ResponseTimeout, FailFastError};
use pin_project::{pin_project, project};
use std::pin::Pin;
use std::task::{Context, Poll};
use tonic::{self as grpc, Code};
use tracing::{debug, warn};

pub fn layer() -> respond::RespondLayer<NewRespond> {
    respond::RespondLayer::new(NewRespond(()))
}

#[derive(Clone, Default)]
pub struct Metrics(metrics::Registry<Label>);

pub type MetricsLayer = metrics::RecordErrorLayer<LabelError, Label>;

/// Error metric labels.
#[derive(Copy, Clone, Debug)]
pub struct LabelError(super::metric_labels::Direction);

pub type Label = (super::metric_labels::Direction, Reason);

#[derive(Copy, Clone, Debug)]
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

#[derive(Copy, Clone, Debug)]
pub enum Respond {
    Http1(http::Version),
    Http2 { is_grpc: bool },
}

#[pin_project]
pub enum ResponseBody<B> {
    NonGrpc(#[pin] B),
    Grpc {
        #[pin]
        inner: B,
        trailers: Option<http::HeaderMap>,
    },
}

impl<B: hyper::body::HttpBody> hyper::body::HttpBody for ResponseBody<B>
where
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = B::Error;

    #[project]
    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        #[project]
        match self.project() {
            ResponseBody::NonGrpc(inner) => inner.poll_data(cx),
            ResponseBody::Grpc { inner, trailers } => {
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

    #[project]
    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        #[project]
        match self.project() {
            ResponseBody::NonGrpc(inner) => inner.poll_trailers(cx),
            ResponseBody::Grpc { inner, trailers } => match trailers.take() {
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
        match req.version() {
            http::Version::HTTP_2 => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok().map(|s| s.starts_with("application/grpc")))
                    .unwrap_or(false);
                Respond::Http2 { is_grpc }
            }
            version => Respond::Http1(version),
        }
    }
}

impl<RspB: Default + hyper::body::HttpBody> respond::Respond<http::Response<RspB>> for Respond {
    type Response = http::Response<ResponseBody<RspB>>;

    fn respond(
        &self,
        reseponse: Result<http::Response<RspB>, Error>,
    ) -> Result<Self::Response, Error> {
        match reseponse {
            Ok(response) => Ok(response.map(|b| match *self {
                Respond::Http2 { is_grpc } if is_grpc == true => ResponseBody::Grpc {
                    inner: b,
                    trailers: None,
                },
                _ => ResponseBody::NonGrpc(b),
            })),
            Err(error) => {
                warn!("Failed to proxy request: {}", error);

                if let Respond::Http2 { is_grpc } = self {
                    if let Some(reset) = error.h2_reason() {
                        debug!(%reset, "Propagating HTTP2 reset");
                        return Err(error);
                    }

                    if *is_grpc {
                        let mut rsp = http::Response::builder()
                            .version(http::Version::HTTP_2)
                            .header(http::header::CONTENT_LENGTH, "0")
                            .body(ResponseBody::default())
                            .expect("app::errors response is valid");
                        let code = set_grpc_status(&*error, rsp.headers_mut());
                        debug!(?code, "Handling error with gRPC status");
                        return Ok(rsp);
                    }
                }

                let version = match self {
                    Respond::Http1(ref version) => version.clone(),
                    Respond::Http2 { .. } => http::Version::HTTP_2,
                };

                let status = http_status(&*error);
                debug!(%status, ?version, "Handling error with HTTP response");
                Ok(http::Response::builder()
                    .version(version)
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
    } else if error.is::<FailFastError>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<tower::timeout::error::Elapsed>() {
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
    const GRPC_STATUS: &'static str = "grpc-status";
    const GRPC_MESSAGE: &'static str = "grpc-message";

    if let Some(HttpError { grpc, message, .. }) = error.downcast_ref::<HttpError>() {
        headers.insert(GRPC_STATUS, code_header(*grpc));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static(message));
        *grpc
    } else if error.is::<ResponseTimeout>() {
        let code = Code::DeadlineExceeded;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static("request timed out"));
        code
    } else if error.is::<FailFastError>() {
        let code = Code::Unavailable;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_static("proxy max-concurrency exhausted"),
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
    pub required: identity::Name,
    pub found: Option<identity::Name>,
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

impl metrics::LabelError<Error> for LabelError {
    type Labels = Label;

    fn label_error(&self, err: &Error) -> Self::Labels {
        (self.0, Self::reason(err.as_ref()))
    }
}

impl metrics::FmtLabels for Reason {
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

impl Metrics {
    pub fn inbound(&self) -> MetricsLayer {
        self.0
            .layer(LabelError(super::metric_labels::Direction::In))
    }

    pub fn outbound(&self) -> MetricsLayer {
        self.0
            .layer(LabelError(super::metric_labels::Direction::Out))
    }

    pub fn report(&self) -> metrics::Registry<Label> {
        self.0.clone()
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

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for HttpError {}
