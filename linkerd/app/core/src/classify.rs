use crate::profiles;
use linkerd_error::Error;
use linkerd_proxy_client_policy as client_policy;
use linkerd_proxy_http::{classify, HasH2Reason, ResponseTimeoutError};
use std::borrow::Cow;
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
    GrpcOpen(client_policy::grpc::Codes),
    ProfileUnmatched(Class),
}

pub type Result<T> = std::result::Result<T, T>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Class {
    Http(Result<http::StatusCode>),
    Grpc(Result<grpc::Code>),
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

            Self::ClientPolicy(ClientPolicy::Http(policy)) => Response::Http(policy.clone()),
            Self::ClientPolicy(ClientPolicy::Grpc(policy)) => Response::Grpc(policy.clone()),

            Self::Default => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|ct| ct.starts_with("application/grpc"))
                    .unwrap_or(false);
                if is_grpc {
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
        for class in classes {
            if class.is_match(rsp) {
                let res = if class.is_failure() {
                    Err(rsp.status())
                } else {
                    Ok(rsp.status())
                };
                return Some(Class::Http(res));
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
            Self::Http(statuses) => Eos::Class(Class::Http(if statuses.contains(status) {
                Err(status)
            } else {
                Ok(status)
            })),

            Self::Grpc(codes) => grpc_code(rsp.headers())
                .map(|c| Eos::Class(Class::Grpc(if codes.contains(c) { Err(c) } else { Ok(c) })))
                .unwrap_or(Eos::GrpcOpen(codes)),

            Self::Profile(ref classes) => Self::match_class(rsp, classes.as_ref())
                .map(Eos::Class)
                .unwrap_or_else(|| {
                    if let Some(code) = grpc_code(rsp.headers()) {
                        let codes = client_policy::grpc::Codes::default();
                        return Eos::Class(Class::Grpc(if codes.contains(code) {
                            Err(code)
                        } else {
                            Ok(code)
                        }));
                    }

                    let http = Class::Http(if status.is_server_error() {
                        Err(status)
                    } else {
                        Ok(status)
                    });
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
            Self::GrpcOpen(codes) => {
                let code = match trailers.and_then(grpc_code) {
                    None => return Class::Grpc(Ok(grpc::Code::Unknown)),
                    Some(code) => code,
                };
                if codes.contains(code) {
                    return Class::Grpc(Err(code));
                }
                Class::Grpc(Ok(code))
            }
            Self::ProfileUnmatched(http_class) => {
                if let Some(code) = trailers.and_then(grpc_code) {
                    let codes = client_policy::grpc::Codes::default();
                    return Class::Grpc(if codes.contains(code) {
                        Err(code)
                    } else {
                        Ok(code)
                    });
                }
                http_class
            }
        }
    }

    fn error(self, err: &Error) -> Class {
        Class::Error(h2_error(err).into())
    }
}

fn grpc_code(hdrs: &http::HeaderMap) -> Option<grpc::Code> {
    hdrs.get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u16>().ok())
        .map(|code| grpc::Code::from_i32(code as i32))
}

fn h2_error(err: &Error) -> String {
    if let Some(reason) = err.h2_reason() {
        // This should output the error code in the same format as the spec,
        // for example: PROTOCOL_ERROR
        format!("h2({:?})", reason)
    } else {
        trace!("classifying found non-h2 error: {:?}", err);
        String::from("unclassified")
    }
}

// === impl Class ===

impl Class {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Class::Http(Err(_)) | Class::Grpc(Err(_)) | Class::Error(_),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Class;
    use http::{HeaderMap, Response, StatusCode};
    use linkerd_proxy_http::classify::{ClassifyEos, ClassifyResponse};

    #[test]
    fn http_response_status_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, Class::Http(Ok(http::StatusCode::OK)));
    }

    #[test]
    fn http_response_status_bad_request() {
        let rsp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, Class::Http(Ok(StatusCode::BAD_REQUEST)));
    }

    #[test]
    fn http_response_status_server_error() {
        let rsp = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let class = super::Response::default().start(&rsp).eos(None);
        assert_eq!(class, Class::Http(Err(StatusCode::INTERNAL_SERVER_ERROR)));
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
        assert_eq!(class, Class::Grpc(Ok(tonic::Code::Ok)));
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
        assert_eq!(class, Class::Grpc(Err(tonic::Code::Unknown)));
    }

    #[test]
    fn grpc_response_trailer_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 0.into());

        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(Ok(tonic::Code::Ok)));
    }

    #[test]
    fn grpc_response_trailer_error() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 4.into());

        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(Err(tonic::Code::DeadlineExceeded)));
    }

    #[test]
    fn grpc_response_trailer_missing() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let trailers = HeaderMap::new();
        let class = super::Response::Grpc(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(Ok(tonic::Code::Unknown)));
    }

    #[test]
    fn profile_without_response_match_falls_back_to_grpc() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 4.into());

        let class = super::Response::Profile(Default::default())
            .start(&rsp)
            .eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(Err(tonic::Code::DeadlineExceeded)));
    }
}
