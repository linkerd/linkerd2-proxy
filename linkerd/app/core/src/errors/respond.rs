use super::{ConnectTimeout, HttpError};
use http::{header::HeaderValue, StatusCode};
use linkerd_error::Error;
use linkerd_error_respond as respond;
use linkerd_proxy_http::{ClientHandle, HasH2Reason};
use linkerd_timeout::{FailFastError, ResponseTimeout};
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tonic::{self as grpc, Code};
use tracing::{debug, warn};

pub const L5D_PROXY_ERROR: &str = "l5d-proxy-error";

pub fn layer() -> respond::RespondLayer<NewRespond> {
    respond::RespondLayer::new(NewRespond(()))
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

// === impl NewRespond ===

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

// === impl Respond ===

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

                // Set the l5d error header on all responses.
                let mut builder = http::Response::builder();
                builder = set_l5d_proxy_error_header(builder, &*error);

                if self.is_grpc {
                    let mut rsp = builder
                        .version(http::Version::HTTP_2)
                        .header(http::header::CONTENT_LENGTH, "0")
                        .header(http::header::CONTENT_TYPE, GRPC_CONTENT_TYPE)
                        .body(ResponseBody::default())
                        .expect("app::errors response is valid");
                    let code = set_grpc_status(&*error, rsp.headers_mut());
                    debug!(?code, "Handling error with gRPC status");
                    return Ok(rsp);
                }

                let rsp = set_http_status(builder, &*error)
                    .version(self.version)
                    .header(http::header::CONTENT_LENGTH, "0")
                    .body(ResponseBody::default())
                    .expect("error response must be valid");
                let status = rsp.status();
                debug!(%status, version = ?self.version, "Handling error with HTTP response");
                Ok(rsp)
            }
        }
    }
}

// === impl ResponseBody ===

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

// === helpers ===

fn set_l5d_proxy_error_header(
    builder: http::response::Builder,
    error: &(dyn std::error::Error + 'static),
) -> http::response::Builder {
    if error.is::<ResponseTimeout>() {
        builder.header(
            L5D_PROXY_ERROR,
            HeaderValue::from_static("request timed out"),
        )
    } else if error.is::<ConnectTimeout>() {
        builder.header(
            L5D_PROXY_ERROR,
            HeaderValue::from_static("failed to connect"),
        )
    } else if let Some(e) = error.downcast_ref::<FailFastError>() {
        builder.header(
            L5D_PROXY_ERROR,
            HeaderValue::from_str(&e.to_string()).unwrap_or_else(|error| {
                warn!(%error, "Failed to encode fail-fast error message");
                HeaderValue::from_static("service in fail-fast")
            }),
        )
    } else if let Some(HttpError { source, .. }) = error.downcast_ref() {
        builder.header(
            L5D_PROXY_ERROR,
            HeaderValue::from_str(&source.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("an error occurred")),
        )
    } else if let Some(source) = error.source() {
        set_l5d_proxy_error_header(builder, source)
    } else {
        builder.header(
            L5D_PROXY_ERROR,
            HeaderValue::from_static("proxy received invalid response"),
        )
    }
}

fn set_http_status(
    builder: http::response::Builder,
    error: &(dyn std::error::Error + 'static),
) -> http::response::Builder {
    if error.is::<ResponseTimeout>() || error.is::<ConnectTimeout>() {
        builder.status(StatusCode::GATEWAY_TIMEOUT)
    } else if error.is::<FailFastError>() {
        builder.status(StatusCode::SERVICE_UNAVAILABLE)
    } else if let Some(HttpError { http_status, .. }) = error.downcast_ref() {
        builder.status(http_status)
    } else if let Some(source) = error.source() {
        set_http_status(builder, source)
    } else {
        builder.status(StatusCode::BAD_GATEWAY)
    }
}

fn set_grpc_status(
    error: &(dyn std::error::Error + 'static),
    headers: &mut http::HeaderMap,
) -> grpc::Code {
    const GRPC_STATUS: &str = "grpc-status";
    const GRPC_MESSAGE: &str = "grpc-message";

    if error.is::<ResponseTimeout>() {
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
    } else if error.is::<std::io::Error>() {
        let code = Code::Unavailable;
        headers.insert(GRPC_STATUS, code_header(code));
        headers.insert(GRPC_MESSAGE, HeaderValue::from_static("connection closed"));
        code
    } else if let Some(HttpError {
        source,
        grpc_status,
        ..
    }) = error.downcast_ref()
    {
        headers.insert(GRPC_STATUS, code_header(*grpc_status));
        if let Ok(v) = HeaderValue::from_str(&*source.to_string()) {
            headers.insert(GRPC_MESSAGE, v);
        }
        *grpc_status
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
    }
}
