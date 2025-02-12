use super::{
    body::ResponseBody,
    header::{GRPC_CONTENT_TYPE, GRPC_MESSAGE, GRPC_STATUS, L5D_PROXY_CONNECTION, L5D_PROXY_ERROR},
};
use crate::svc;
use http::header::{HeaderValue, LOCATION};
use linkerd_error::{Error, Result};
use linkerd_error_respond as respond;
use linkerd_proxy_http::{orig_proto, ClientHandle};
use linkerd_stack::ExtractParam;
use std::borrow::Cow;
use tracing::{debug, info_span, warn};

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
    http_status: http::StatusCode,
    close_connection: bool,
    pub message: Cow<'static, str>,
    location: Option<HeaderValue>,
}

#[derive(Copy, Clone, Debug)]
pub struct EmitHeaders(pub bool);

#[derive(Clone, Debug)]
pub struct ExtractRespond<P>(P);

#[derive(Copy, Clone, Debug)]
pub struct NewRespond<R> {
    rescue: R,
    emit_headers: bool,
}

#[derive(Clone, Debug)]
pub struct Respond<R> {
    rescue: R,
    version: http::Version,
    is_grpc: bool,
    is_orig_proto_upgrade: bool,
    client: Option<ClientHandle>,
    emit_headers: bool,
}

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
        Self::internal_error("unexpected error")
    }

    pub fn internal_error(msg: impl Into<Cow<'static, str>>) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::INTERNAL_SERVER_ERROR,
            grpc_status: tonic::Code::Internal,
            message: msg.into(),
            location: None,
        }
    }

    pub fn bad_gateway(msg: impl ToString) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::BAD_GATEWAY,
            grpc_status: tonic::Code::Unavailable,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn gateway_timeout(msg: impl ToString) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::GATEWAY_TIMEOUT,
            grpc_status: tonic::Code::DeadlineExceeded,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn gateway_timeout_nonfatal(msg: impl ToString) -> Self {
        Self {
            close_connection: false,
            http_status: http::StatusCode::GATEWAY_TIMEOUT,
            grpc_status: tonic::Code::DeadlineExceeded,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn unavailable(msg: impl ToString) -> Self {
        Self {
            close_connection: true,
            http_status: http::StatusCode::SERVICE_UNAVAILABLE,
            grpc_status: tonic::Code::Unavailable,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn unauthenticated(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::FORBIDDEN,
            grpc_status: tonic::Code::Unauthenticated,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn permission_denied(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::FORBIDDEN,
            grpc_status: tonic::Code::PermissionDenied,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn rate_limited(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::TOO_MANY_REQUESTS,
            grpc_status: tonic::Code::ResourceExhausted,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn loop_detected(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::LOOP_DETECTED,
            grpc_status: tonic::Code::Aborted,
            close_connection: true,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn not_found(msg: impl ToString) -> Self {
        Self {
            http_status: http::StatusCode::NOT_FOUND,
            grpc_status: tonic::Code::NotFound,
            close_connection: false,
            message: Cow::Owned(msg.to_string()),
            location: None,
        }
    }

    pub fn redirect(http_status: http::StatusCode, location: &http::Uri) -> Self {
        Self {
            http_status,
            grpc_status: tonic::Code::NotFound,
            close_connection: false,
            message: Cow::Borrowed("redirected"),
            location: Some(
                HeaderValue::try_from(location.to_string())
                    .expect("location must be a valid header value"),
            ),
        }
    }

    pub fn response(http_status: http::StatusCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            http_status,
            location: None,
            grpc_status: tonic::Code::FailedPrecondition,
            close_connection: false,
            message: message.into(),
        }
    }

    pub fn grpc(grpc_status: tonic::Code, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            grpc_status,
            http_status: http::StatusCode::OK,
            location: None,
            close_connection: false,
            message: message.into(),
        }
    }

    #[inline]
    fn message(&self) -> HeaderValue {
        match self.message {
            Cow::Borrowed(msg) => HeaderValue::from_static(msg),
            Cow::Owned(ref msg) => HeaderValue::from_str(msg).unwrap_or_else(|error| {
                warn!(%error, "Failed to encode error header");
                HeaderValue::from_static("unexpected error")
            }),
        }
    }

    #[inline]
    fn grpc_response<B: Default>(&self, emit_headers: bool) -> http::Response<B> {
        debug!(code = %self.grpc_status, "Handling error on gRPC connection");
        let mut rsp = http::Response::builder()
            .version(http::Version::HTTP_2)
            .header(http::header::CONTENT_LENGTH, "0")
            .header(http::header::CONTENT_TYPE, GRPC_CONTENT_TYPE)
            .header(GRPC_STATUS, super::code_header(self.grpc_status));

        if emit_headers {
            rsp = rsp
                .header(GRPC_MESSAGE, self.message())
                .header(L5D_PROXY_ERROR, self.message());
        }

        if self.close_connection && emit_headers {
            // TODO only set when meshed.
            rsp = rsp.header(L5D_PROXY_CONNECTION, "close");
        }

        rsp.body(B::default())
            .expect("error response must be valid")
    }

    #[inline]
    fn http_response<B: Default>(
        &self,
        version: http::Version,
        emit_headers: bool,
        is_orig_proto_upgrade: bool,
    ) -> http::Response<B> {
        debug!(
            status = %self.http_status,
            ?version,
            close = %self.close_connection,
            "Handling error on HTTP connection"
        );
        let mut rsp = http::Response::builder()
            .status(self.http_status)
            .version(version)
            .header(http::header::CONTENT_LENGTH, "0");

        if emit_headers {
            rsp = rsp.header(L5D_PROXY_ERROR, self.message());
        }

        if self.close_connection {
            if version == http::Version::HTTP_11 && !is_orig_proto_upgrade {
                // Notify the (proxy or non-proxy) client that the connection will be closed.
                rsp = rsp.header(http::header::CONNECTION, "close");
            }

            // Tell the remote outbound proxy that it should close the proxied connection to its
            // application, i.e. so the application can choose another replica.
            if emit_headers {
                // TODO only set when meshed.
                rsp = rsp.header(L5D_PROXY_CONNECTION, "close");
            }
        }

        if let Some(loc) = &self.location {
            rsp = rsp.header(LOCATION, loc);
        }

        rsp.body(B::default())
            .expect("error response must be valid")
    }
}

// === impl ExtractRespond ===

impl<T, R, P> ExtractParam<NewRespond<R>, T> for ExtractRespond<P>
where
    P: ExtractParam<R, T>,
    P: ExtractParam<EmitHeaders, T>,
{
    #[inline]
    fn extract_param(&self, t: &T) -> NewRespond<R> {
        let EmitHeaders(emit_headers) = self.0.extract_param(t);
        NewRespond {
            rescue: self.0.extract_param(t),
            emit_headers,
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
        let emit_headers = self.emit_headers;

        match req.version() {
            http::Version::HTTP_2 => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| {
                        v.to_str().ok().map(|s| {
                            s.starts_with(
                                GRPC_CONTENT_TYPE
                                    .to_str()
                                    .expect("GRPC_CONTENT_TYPE only contains visible ASCII"),
                            )
                        })
                    })
                    .unwrap_or(false);
                Respond {
                    client,
                    rescue,
                    is_grpc,
                    is_orig_proto_upgrade: false,
                    version: http::Version::HTTP_2,
                    emit_headers,
                }
            }
            version => {
                let is_h2_upgrade = req.extensions().get::<orig_proto::WasUpgrade>().is_some();
                Respond {
                    client,
                    rescue,
                    version,
                    is_grpc: false,
                    is_orig_proto_upgrade: is_h2_upgrade,
                    emit_headers,
                }
            }
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
    B: Default + linkerd_proxy_http::Body,
    R: HttpRescue<Error> + Clone,
{
    type Response = http::Response<ResponseBody<R, B>>;

    fn respond(&self, res: Result<http::Response<B>>) -> Result<Self::Response> {
        let error = match res {
            Ok(rsp) => {
                return Ok(rsp.map(|inner| match self {
                    Respond {
                        is_grpc: true,
                        rescue,
                        emit_headers,
                        ..
                    } => ResponseBody::grpc_rescue(inner, rescue.clone(), *emit_headers),
                    _ => ResponseBody::passthru(inner),
                }));
            }
            Err(error) => error,
        };

        let rsp = info_span!("rescue", client.addr = %self.client_addr()).in_scope(|| {
            if !self.is_grpc {
                let version = self.version;
                tracing::info!(error, "{version:?} request failed",);
            } else {
                tracing::info!(error, "gRPC request failed");
            };
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
            rsp.grpc_response(self.emit_headers)
        } else {
            rsp.http_response(self.version, self.emit_headers, self.is_orig_proto_upgrade)
        };

        Ok(rsp)
    }
}
