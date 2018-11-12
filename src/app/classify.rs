use h2;
use http;

pub use proxy::http::metrics::classify::{self, layer, CanClassify};
use proxy::http::profiles;

#[derive(Clone, Debug)]
pub enum Request {
    Default,
    Profile(profiles::ResponseClasses),
}

#[derive(Clone, Debug)]
pub enum Response {
    Default,
    Grpc,
    Profile(profiles::ResponseClasses),
}

#[derive(Clone, Debug)]
pub enum Eos {
    Default(http::StatusCode),
    Grpc(GrpcEos),
    Profile(Class),
}

#[derive(Clone, Debug)]
pub enum GrpcEos {
    NoBody(Class),
    Open,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Class {
    Default(SuccessOrFailure),
    Grpc(SuccessOrFailure, u32),
    Stream(SuccessOrFailure, String),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SuccessOrFailure {
    Success,
    Failure,
}

// === impl Request ===

impl From<profiles::ResponseClasses> for Request {
    fn from(classes: profiles::ResponseClasses) -> Self {
        if classes.is_empty() {
            Request::Default
        } else {
            Request::Profile(classes)
        }
    }
}

impl Default for Request {
    fn default() -> Self {
        Request::Default
    }
}

impl classify::Classify for Request {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyResponse = Response;
    type ClassifyEos = Eos;

    fn classify<B>(&self, req: &http::Request<B>) -> Self::ClassifyResponse {
        match self {
            Request::Profile(classes) => Response::Profile(classes.clone()),
            Request::Default => {
                let is_grpc = req
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|ct| ct.starts_with("application/grpc+"))
                    .unwrap_or(false);

                if is_grpc {
                    Response::Grpc
                } else {
                    Response::Default
                }
            }
        }
    }
}

// === impl Response ===

impl Default for Response {
    fn default() -> Self {
        // By default, simply perform HTTP classification. This only applies
        // when no `insert` layer is present.
        Response::Default
    }
}

impl Response {
    fn match_class<B>(
        rsp: &http::Response<B>,
        classes: &[profiles::ResponseClass],
    ) -> Option<Class> {
        for class in classes {
            if class.is_match(rsp) {
                let result = if class.is_failure() {
                    SuccessOrFailure::Failure
                } else {
                    SuccessOrFailure::Success
                };
                return Some(Class::Default(result));
            }
        }

        None
    }
}

impl classify::ClassifyResponse for Response {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyEos = Eos;

    fn start<B>(self, rsp: &http::Response<B>) -> Eos {
        match self {
            Response::Default => grpc_class(rsp.headers())
                .map(|c| Eos::Grpc(GrpcEos::NoBody(c)))
                .unwrap_or_else(|| Eos::Default(rsp.status())),
            Response::Grpc => grpc_class(rsp.headers())
                .map(|c| Eos::Grpc(GrpcEos::NoBody(c)))
                .unwrap_or(Eos::Grpc(GrpcEos::Open)),
            Response::Profile(ref classes) => Self::match_class(rsp, classes.as_ref())
                .map(Eos::Profile)
                .unwrap_or_else(|| {
                    grpc_class(rsp.headers())
                        .map(|c| Eos::Grpc(GrpcEos::NoBody(c)))
                        .unwrap_or_else(|| Eos::Default(rsp.status()))
                }),
        }
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        Class::Stream(SuccessOrFailure::Failure, format!("{}", err))
    }
}

// === impl Eos ===

impl classify::ClassifyEos for Eos {
    type Class = Class;
    type Error = h2::Error;

    fn eos(self, trailers: Option<&http::HeaderMap>) -> Self::Class {
        match self {
            Eos::Default(status) if status.is_server_error() => {
                Class::Default(SuccessOrFailure::Failure)
            }
            Eos::Default(_) => trailers
                .and_then(grpc_class)
                .unwrap_or_else(|| Class::Default(SuccessOrFailure::Success)),
            Eos::Grpc(GrpcEos::NoBody(class)) => class,
            Eos::Grpc(GrpcEos::Open) => trailers
                .and_then(grpc_class)
                .unwrap_or_else(|| Class::Grpc(SuccessOrFailure::Failure, 0)),
            Eos::Profile(class) => class,
        }
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        Class::Stream(SuccessOrFailure::Failure, format!("{}", err))
    }
}

fn grpc_class(headers: &http::HeaderMap) -> Option<Class> {
    headers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        .map(|grpc_status| {
            let ok = if grpc_status == 0 {
                SuccessOrFailure::Success
            } else {
                SuccessOrFailure::Failure
            };
            Class::Grpc(ok, grpc_status)
        })
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Response, StatusCode};

    use super::{Class, SuccessOrFailure};
    use proxy::http::metrics::classify::{ClassifyEos as _CE, ClassifyResponse as _CR};

    #[test]
    fn http_response_status_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let class = super::Response::Default.start(&rsp).eos(None);
        assert_eq!(class, Class::Default(SuccessOrFailure::Success));
    }

    #[test]
    fn http_response_status_bad_request() {
        let rsp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let class = super::Response::Default.start(&rsp).eos(None);
        assert_eq!(class, Class::Default(SuccessOrFailure::Success));
    }

    #[test]
    fn http_response_status_server_error() {
        let rsp = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let class = super::Response::Default.start(&rsp).eos(None);
        assert_eq!(class, Class::Default(SuccessOrFailure::Failure));
    }

    #[test]
    fn grpc_response_header_ok() {
        let rsp = Response::builder()
            .header("grpc-status", "0")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let class = super::Response::Grpc.start(&rsp).eos(None);
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Success, 0));
    }

    #[test]
    fn grpc_response_header_error() {
        let rsp = Response::builder()
            .header("grpc-status", "2")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let class = super::Response::Grpc.start(&rsp).eos(None);
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 2));
    }

    #[test]
    fn grpc_response_trailer_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 0.into());

        let class = super::Response::Grpc.start(&rsp).eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Success, 0));
    }

    #[test]
    fn grpc_response_trailer_error() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 3.into());

        let class = super::Response::Grpc.start(&rsp).eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 3));
    }

    #[test]
    fn profile_without_response_match_falls_back_to_grpc() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 3.into());

        let class = super::Response::Profile(Default::default()).start(&rsp).eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 3));
    }
}
