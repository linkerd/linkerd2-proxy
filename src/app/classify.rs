use h2;
use http;

use proxy::http::classify;

pub use proxy::http::classify::insert;

#[derive(Clone, Debug, Default)]
pub struct Request {}

#[derive(Clone, Debug)]
pub enum Response {
    Grpc,
    Http,
}

#[derive(Clone, Debug)]
pub enum Eos {
    Http(HttpEos),
    Grpc(GrpcEos),
}

#[derive(Clone, Debug)]
pub struct HttpEos(http::StatusCode);

#[derive(Clone, Debug)]
pub enum GrpcEos {
    NoBody(Class),
    Open,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Class {
    Grpc(SuccessOrFailure, u32),
    Http(SuccessOrFailure),
    Stream(SuccessOrFailure, String),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SuccessOrFailure {
    Success,
    Failure,
}

// === impl Request ===

impl classify::Classify for Request {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyResponse = Response;
    type ClassifyEos = Eos;

    fn classify<B>(&self, req: &http::Request<B>) -> Self::ClassifyResponse {
        // Determine if the request is a gRPC request by checking the content-type.
        if let Some(ref ct) = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
        {
            if ct.starts_with("application/grpc+") {
                return Response::Grpc;
            }
        }

        Response::Http {}
    }
}

// === impl Response ===

impl Default for Response {
    fn default() -> Self {
        Response::Http
    }
}

impl classify::ClassifyResponse for Response {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyEos = Eos;

    fn start<B>(self, rsp: &http::Response<B>) -> Eos {
        match self {
            Response::Http => Eos::Http(HttpEos(rsp.status())),
            Response::Grpc => Eos::Grpc(match grpc_class(rsp.headers()) {
                None => GrpcEos::Open,
                Some(class) => GrpcEos::NoBody(class.clone()),
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
            Eos::Http(http) => http.eos(trailers),
            Eos::Grpc(grpc) => grpc.eos(trailers),
        }
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        match self {
            Eos::Http(http) => http.error(err),
            Eos::Grpc(grpc) => grpc.error(err),
        }
    }
}

impl classify::ClassifyEos for HttpEos {
    type Class = Class;
    type Error = h2::Error;

    fn eos(self, _: Option<&http::HeaderMap>) -> Self::Class {
        match self {
            HttpEos(status) if status.is_server_error() => Class::Http(SuccessOrFailure::Failure),
            HttpEos(_) => Class::Http(SuccessOrFailure::Success),
        }
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        Class::Stream(SuccessOrFailure::Failure, format!("{}", err))
    }
}

impl classify::ClassifyEos for GrpcEos {
    type Class = Class;
    type Error = h2::Error;

    fn eos(self, trailers: Option<&http::HeaderMap>) -> Self::Class {
        match self {
            GrpcEos::NoBody(class) => class,
            GrpcEos::Open => trailers
                .and_then(grpc_class)
                .unwrap_or_else(|| Class::Grpc(SuccessOrFailure::Success, 0)),
        }
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        // Ignore the original classification when an error is encountered.
        Class::Stream(SuccessOrFailure::Failure, format!("{}", err))
    }
}

fn grpc_class(headers: &http::HeaderMap) -> Option<Class> {
    headers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        .map(|grpc_status| {
            if grpc_status == 0 {
                Class::Grpc(SuccessOrFailure::Success, grpc_status)
            } else {
                Class::Grpc(SuccessOrFailure::Failure, grpc_status)
            }
        })
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Response, StatusCode};

    use super::{Class, SuccessOrFailure};
    use proxy::http::classify::{ClassifyEos as _CE, ClassifyResponse as _CR};

    #[test]
    fn http_response_status_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = super::Response::default();
        let ceos = crsp.start(&rsp);
        let class = ceos.eos(None);
        assert_eq!(class, Class::Http(SuccessOrFailure::Success));
    }

    #[test]
    fn http_response_status_bad_request() {
        let rsp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let crsp = super::Response::default();
        let ceos = crsp.start(&rsp);
        let class = ceos.eos(None);
        assert_eq!(class, Class::Http(SuccessOrFailure::Success));
    }

    #[test]
    fn http_response_status_server_error() {
        let rsp = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let crsp = super::Response::default();
        let ceos = crsp.start(&rsp);
        let class = ceos.eos(None);
        assert_eq!(class, Class::Http(SuccessOrFailure::Failure));
    }

    #[test]
    fn grpc_response_header_ok() {
        let rsp = Response::builder()
            .header("grpc-status", "0")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let crsp = super::Response::Grpc;
        let ceos = crsp.start(&rsp);
        let class = ceos.eos(None);
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Success, 0));
    }

    #[test]
    fn grpc_response_header_error() {
        let rsp = Response::builder()
            .header("grpc-status", "2")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let crsp = super::Response::Grpc;
        let ceos = crsp.start(&rsp);
        let class = ceos.eos(None);
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 2));
    }

    #[test]
    fn grpc_response_trailer_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = super::Response::Grpc;
        let ceos = crsp.start(&rsp);

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 0.into());

        let class = ceos.eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Success, 0));
    }

    #[test]
    fn grpc_response_trailer_error() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = super::Response::Grpc;
        let ceos = crsp.start(&rsp);

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", 3.into());

        let class = ceos.eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 3));
    }
}
