use crate::profiles;
use linkerd_error::Error;
use linkerd_proxy_client_policy as client_policy;
use linkerd_proxy_http::{classify, HasH2Reason, ResponseTimeoutError};
use std::{borrow::Cow, time::Duration};
use tonic as grpc;
use tracing::trace;

pub type NewClassify<N, X = ()> = classify::NewInsertClassifyResponse<Request, X, N>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Request {
    Default,
    Profile(profiles::http::ResponseClasses),
    ClientPolicy(ClientPolicy),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Response {
    Http(client_policy::http::StatusRanges),
    Grpc(client_policy::grpc::Codes),
    Profile(profiles::http::ResponseClasses),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ClientPolicy {
    Grpc(client_policy::grpc::Codes),
    Http(client_policy::http::StatusRanges),
}

#[derive(Clone, Debug)]
pub enum Eos {
    Class(Class),
    GrpcOpen {
        codes: client_policy::grpc::Codes,
        retry_after_hint: Option<Duration>,
    },
    ProfileUnmatched(Class),
}

pub type Result<T> = std::result::Result<T, T>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Class {
    Http {
        status: Result<http::StatusCode>,
        retry_after_hint: Option<Duration>,
    },
    Grpc {
        code: Result<grpc::Code>,
        retry_after_hint: Option<Duration>,
    },
    Error(Cow<'static, str>),
}

// === impl Request ===

impl From<profiles::http::ResponseClasses> for Request {
    fn from(classes: profiles::http::ResponseClasses) -> Self {
        if !classes.is_empty() {
            return Self::Profile(classes);
        }
        Self::Default
    }
}

impl Default for Request {
    fn default() -> Self {
        Self::Default
    }
}

impl classify::Classify for Request {
    type Class = Class;
    type ClassifyResponse = Response;
    type ClassifyEos = Eos;

    fn classify<B>(&self, req: &http::Request<B>) -> Self::ClassifyResponse {
        match self {
            Self::Profile(classes) => Response::Profile(classes.clone()),

            Self::ClientPolicy(ClientPolicy::Http(policy)) => {
                if is_grpc(req.headers()) {
                    return Response::Grpc(Default::default());
                }
                Response::Http(policy.clone())
            }
            Self::ClientPolicy(ClientPolicy::Grpc(policy)) => Response::Grpc(policy.clone()),

            Self::Default => {
                if is_grpc(req.headers()) {
                    return Response::Grpc(Default::default());
                }
                Response::default()
            }
        }
    }
}

// === impl Response ===

impl Default for Response {
    fn default() -> Self {
        Self::Http(client_policy::http::StatusRanges::default())
    }
}

impl Response {
    fn match_class<B>(
        rsp: &http::Response<B>,
        classes: &[profiles::http::ResponseClass],
    ) -> Option<Class> {
        let retry_after_hint = http_retry_after_hint(rsp.status(), rsp.headers());

        for class in classes {
            if class.is_match(rsp) {
                let res = if class.is_failure() {
                    Err(rsp.status())
                } else {
                    Ok(rsp.status())
                };
                return Some(Class::Http {
                    status: res,
                    retry_after_hint,
                });
            }
        }

        None
    }
}

impl classify::ClassifyResponse for Response {
    type Class = Class;
    type ClassifyEos = Eos;

    fn start<B>(self, rsp: &http::Response<B>) -> Eos {
        let status = rsp.status();
        match self {
            Self::Http(statuses) => Eos::Class(Class::Http {
                status: if statuses.contains(status) {
                    Err(status)
                } else {
                    Ok(status)
                },
                retry_after_hint: http_retry_after_hint(status, rsp.headers()),
            }),

            Self::Grpc(codes) => {
                let retry_after_hint = grpc_retry_after_hint(rsp.headers());
                if let Some(code) = grpc_code(rsp.headers()) {
                    Eos::Class(grpc_class(&codes, code, retry_after_hint))
                } else {
                    Eos::GrpcOpen {
                        codes,
                        retry_after_hint,
                    }
                }
            }

            Self::Profile(ref classes) => Self::match_class(rsp, classes.as_ref())
                .map(Eos::Class)
                .unwrap_or_else(|| {
                    if let Some(code) = grpc_code(rsp.headers()) {
                        let codes = client_policy::grpc::Codes::default();
                        return Eos::Class(grpc_class(
                            &codes,
                            code,
                            grpc_retry_after_hint(rsp.headers()),
                        ));
                    }

                    let http = Class::Http {
                        status: if status.is_server_error() {
                            Err(status)
                        } else {
                            Ok(status)
                        },
                        retry_after_hint: http_retry_after_hint(status, rsp.headers()),
                    };
                    Eos::ProfileUnmatched(http)
                }),
        }
    }

    fn error(self, err: &Error) -> Self::Class {
        let msg = if err.is::<ResponseTimeoutError>() {
            "timeout".into()
        } else {
            h2_error(err).into()
        };
        Class::Error(msg)
    }
}

// === impl Eos ===

impl classify::ClassifyEos for Eos {
    type Class = Class;

    fn eos(self, trailers: Option<&http::HeaderMap>) -> Class {
        match self {
            Self::Class(class) => class,
            Self::GrpcOpen {
                codes,
                retry_after_hint,
            } => {
                let retry_after_hint = max_retry_after_hint(
                    retry_after_hint,
                    trailers.and_then(grpc_retry_after_hint),
                );
                let code = match trailers.and_then(grpc_code) {
                    None => {
                        return Class::Grpc {
                            code: Ok(grpc::Code::Unknown),
                            retry_after_hint,
                        };
                    }
                    Some(code) => code,
                };
                if codes.contains(code) {
                    return Class::Grpc {
                        code: Err(code),
                        retry_after_hint,
                    };
                }
                Class::Grpc {
                    code: Ok(code),
                    retry_after_hint,
                }
            }
            Self::ProfileUnmatched(http_class) => {
                if let Some(code) = trailers.and_then(grpc_code) {
                    let codes = client_policy::grpc::Codes::default();
                    return grpc_class(&codes, code, trailers.and_then(grpc_retry_after_hint));
                }
                http_class
            }
        }
    }

    fn error(self, err: &Error) -> Class {
        Class::Error(h2_error(err).into())
    }
}

pub fn grpc_code(hdrs: &http::HeaderMap) -> Option<grpc::Code> {
    hdrs.get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u16>().ok())
        .map(|code| grpc::Code::from_i32(code as i32))
}

fn http_retry_after_hint(status: http::StatusCode, hdrs: &http::HeaderMap) -> Option<Duration> {
    classify::retry_after::parse_retry_after(status, hdrs, Duration::MAX)
}

fn grpc_retry_after_hint(hdrs: &http::HeaderMap) -> Option<Duration> {
    classify::retry_after::parse_grpc_retry_pushback(hdrs, Duration::MAX)
}

fn max_retry_after_hint(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn grpc_class(
    codes: &client_policy::grpc::Codes,
    code: grpc::Code,
    retry_after_hint: Option<Duration>,
) -> Class {
    Class::Grpc {
        code: if codes.contains(code) {
            Err(code)
        } else {
            Ok(code)
        },
        retry_after_hint,
    }
}

fn h2_error(err: &Error) -> String {
    if let Some(reason) = err.h2_reason() {
        // This should output the error code in the same format as the spec,
        // for example: PROTOCOL_ERROR
        format!("h2({reason:?})")
    } else {
        trace!("classifying found non-h2 error: {:?}", err);
        String::from("unclassified")
    }
}

fn is_grpc(hdrs: &http::HeaderMap) -> bool {
    hdrs.get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("application/grpc"))
        .unwrap_or(false)
}

// === impl Class ===

impl Class {
    #[inline]
    pub fn is_success(&self) -> bool {
        !self.is_failure()
    }

    #[inline]
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Class::Http { status: Err(_), .. } | Class::Grpc { code: Err(_), .. } | Class::Error(_),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Class;
    use http::{HeaderMap, Request, Response, StatusCode};
    use linkerd_proxy_http::{
        classify::{ClassifyEos, ClassifyResponse},
        Classify,
    };
    use std::time::Duration;

    fn http(status: super::Result<StatusCode>) -> Class {
        Class::Http {
            status,
            retry_after_hint: None,
        }
    }

    fn grpc(code: super::Result<tonic::Code>) -> Class {
        Class::Grpc {
            code,
            retry_after_hint: None,
        }
    }

    #[test]
    fn http_response_status_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, http(Ok(http::StatusCode::OK)));
    }

    #[test]
    fn http_response_status_bad_request() {
        let rsp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, http(Ok(StatusCode::BAD_REQUEST)));
    }

    #[test]
    fn http_response_status_server_error() {
        let rsp = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, http(Err(StatusCode::INTERNAL_SERVER_ERROR)));
    }

    #[test]
    fn http_response_retry_after_hint() {
        let rsp = Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(http::header::RETRY_AFTER, "5")
            .body(())
            .unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(
            class,
            Class::Http {
                status: Ok(StatusCode::TOO_MANY_REQUESTS),
                retry_after_hint: Some(Duration::from_secs(5)),
            }
        );
    }

    #[test]
    fn grpc_response_header_ok() {
        let rsp = Response::builder()
            .header("grpc-status", "0")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(None);
        assert_eq!(class, grpc(Ok(tonic::Code::Ok)));
    }

    #[test]
    fn grpc_response_header_error() {
        let rsp = Response::builder()
            .header("grpc-status", "2")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(None);
        assert_eq!(class, grpc(Err(tonic::Code::Unknown)));
    }

    #[test]
    fn grpc_response_trailer_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 0.into());

        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, grpc(Ok(tonic::Code::Ok)));
    }

    #[test]
    fn grpc_response_trailer_error() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 4.into());

        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, grpc(Err(tonic::Code::DeadlineExceeded)));
    }

    #[test]
    fn grpc_response_trailer_retry_pushback_hint() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "14".parse().unwrap());
        trailers.insert("grpc-retry-pushback-ms", "2500".parse().unwrap());

        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(
            class,
            Class::Grpc {
                code: Err(tonic::Code::Unavailable),
                retry_after_hint: Some(Duration::from_millis(2500)),
            }
        );
    }

    #[test]
    fn grpc_response_trailer_missing() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let trailers = HeaderMap::new();
        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, grpc(Ok(tonic::Code::Unknown)));
    }

    #[test]
    fn profile_without_response_match_falls_back_to_grpc() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 4.into());

        let class = super::Response::Profile(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, grpc(Err(tonic::Code::DeadlineExceeded)));
    }

    #[test]
    fn grpc_over_http_trailer_ok() {
        let req = Request::builder()
            .header(http::header::CONTENT_TYPE, "application/grpc+proto")
            .body(())
            .unwrap();
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 0.into());
        let class = super::Request::ClientPolicy(super::ClientPolicy::Http(Default::default()))
            .classify(&req)
            .start(&rsp)
            .eos(Some(&trailers));

        assert_eq!(class, grpc(Ok(tonic::Code::Ok)));
    }

    #[test]
    fn grpc_over_http_trailer_error() {
        let req = Request::builder()
            .header(http::header::CONTENT_TYPE, "application/grpc+proto")
            .body(())
            .unwrap();
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 4.into());
        let class = super::Request::ClientPolicy(super::ClientPolicy::Http(Default::default()))
            .classify(&req)
            .start(&rsp)
            .eos(Some(&trailers));

        assert_eq!(class, grpc(Err(tonic::Code::DeadlineExceeded)));
    }
}
