use crate::proxy::identity;
use http::{header::HeaderValue, StatusCode};
use linkerd2_error::Error;
use linkerd2_error_respond as respond;
use linkerd2_proxy_http::HasH2Reason;
use linkerd2_router::error as router;
use tower::load_shed::error as shed;
use tower_grpc::{self as grpc, Code};
use tracing::debug;

pub fn layer<B: Default>() -> respond::RespondLayer<NewRespond<B>> {
    respond::RespondLayer::new(NewRespond(std::marker::PhantomData))
}

#[derive(Debug)]
pub struct NewRespond<B>(std::marker::PhantomData<fn() -> B>);

#[derive(Copy, Clone, Debug)]
pub enum Respond<B> {
    Http1(std::marker::PhantomData<fn() -> B>),
    Http2 { is_grpc: bool },
}

impl<A, B: Default> respond::NewRespond<http::Request<A>> for NewRespond<B> {
    type Response = http::Response<B>;
    type Respond = Respond<B>;

    fn new_respond(&self, req: &http::Request<A>) -> Self::Respond {
        if req.version() == http::Version::HTTP_2 {
            let is_grpc = req
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok().map(|s| s.starts_with("application/grpc")))
                .unwrap_or(false);
            Respond::Http2 { is_grpc }
        } else {
            Respond::Http1(self.0)
        }
    }
}

impl<B> Clone for NewRespond<B> {
    fn clone(&self) -> Self {
        NewRespond(self.0)
    }
}

impl<B: Default> respond::Respond for Respond<B> {
    type Response = http::Response<B>;

    fn respond(&self, error: Error) -> Result<Self::Response, Error> {
        tracing::warn!("Failed to proxy request: {}", error);

        if let Respond::Http2 { is_grpc } = self {
            if let Some(reset) = error.h2_reason() {
                debug!(%reset, "Propagating HTTP2 reset");
                return Err(error);
            }

            if *is_grpc {
                debug!("Handling error with gRPC status code");
                let mut rsp = http::Response::builder()
                    .header(http::header::CONTENT_LENGTH, "0")
                    .body(B::default())
                    .expect("app::errors response is valid");
                set_grpc_status(error, rsp.headers_mut());
                return Ok(rsp);
            }
        }

        let status = http_status(error);
        debug!(%status, "Handling error with HTTP response");
        return Ok(http::Response::builder()
            .status(status)
            .header(http::header::CONTENT_LENGTH, "0")
            .body(B::default())
            .expect("error response must be valid"));
    }
}

fn http_status(error: Error) -> StatusCode {
    if error.is::<router::NoCapacity>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<shed::Overloaded>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<tower::timeout::error::Elapsed>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if error.is::<IdentityRequired>() {
        http::StatusCode::FORBIDDEN
    } else {
        http::StatusCode::BAD_GATEWAY
    }
}

fn set_grpc_status(error: Error, headers: &mut http::HeaderMap) {
    const GRPC_STATUS: &'static str = "grpc-status";
    const GRPC_MESSAGE: &'static str = "grpc-message";

    if error.is::<router::NoCapacity>() {
        headers.insert(GRPC_STATUS, code_header(Code::Unavailable));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_static("proxy router cache exhausted"),
        );
    } else if error.is::<shed::Overloaded>() {
        headers.insert(GRPC_STATUS, code_header(Code::Unavailable));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_static("proxy max-concurrency exhausted"),
        );
    } else if error.is::<tower::timeout::error::Elapsed>() {
        headers.insert(GRPC_STATUS, code_header(Code::Unavailable));
        headers.insert(
            GRPC_MESSAGE,
            HeaderValue::from_static("proxy dispatch timed out"),
        );
    } else if error.is::<IdentityRequired>() {
        headers.insert(GRPC_STATUS, code_header(Code::FailedPrecondition));
        if let Ok(msg) = HeaderValue::from_str(&error.to_string()) {
            headers.insert(GRPC_MESSAGE, msg);
        }
    } else {
        headers.insert(GRPC_STATUS, code_header(Code::Internal));
        if let Ok(msg) = HeaderValue::from_str(&error.to_string()) {
            headers.insert(GRPC_MESSAGE, msg);
        }
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
