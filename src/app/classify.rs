use h2;
use http;

use proxy::http::classify;

#[derive(Clone, Debug, Default)]
pub struct Classify {}

#[derive(Clone, Debug, Default)]
pub struct ClassifyResponse {}

#[derive(Clone, Debug, Default)]
pub struct ClassifyEos {
    class: Option<Class>,
    status: http::StatusCode,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Class {
    Grpc(SuccessOrFailure, u32),
    Http(SuccessOrFailure, http::StatusCode),
    Stream(SuccessOrFailure, String),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SuccessOrFailure {
    Success,
    Failure,
}

// === impl Classify ===

impl classify::Classify for Classify {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyResponse = ClassifyResponse;
    type ClassifyEos = ClassifyEos;

    fn classify<B>(&self, _: &http::Request<B>) -> Self::ClassifyResponse {
        ClassifyResponse {}
    }
}

// === impl ClassifyResponse ===

impl classify::ClassifyResponse for ClassifyResponse {
    type Class = Class;
    type Error = h2::Error;
    type ClassifyEos = ClassifyEos;

    fn start<B>(self, rsp: &http::Response<B>) -> (ClassifyEos, Option<Class>) {
        let class = grpc_class(rsp.headers());
        let eos = ClassifyEos {
            class: class.clone(),
            status: rsp.status(),
        };
        (eos, class)
    }

    fn error(self, err: &h2::Error) -> Self::Class {
        Class::Stream(SuccessOrFailure::Failure, format!("{}", err))
    }
}

impl classify::ClassifyEos for ClassifyEos {
    type Class = Class;
    type Error = h2::Error;

    fn eos(mut self, trailers: Option<&http::HeaderMap>) -> Self::Class {
        // If the response headers already classified this stream, use that.
        if let Some(class) = self.class.take() {
            return class;
        }

        // Otherwise, fall-back to the default classification logic.
        if let Some(class) = trailers.and_then(grpc_class) {
            return class;
        }

        let result = if self.status.is_server_error() {
            SuccessOrFailure::Failure
        } else {
            SuccessOrFailure::Success
        };
        Class::Http(result, self.status)
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

    use super::*;
    use proxy::http::classify::{ClassifyEos as _CE, ClassifyResponse as _CR};

    #[test]
    fn http_response_status_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, None);

        let class = ceos.eos(None);
        assert_eq!(
            class,
            Class::Http(SuccessOrFailure::Success, StatusCode::OK)
        );
    }

    #[test]
    fn http_response_status_bad_request() {
        let rsp = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(())
            .unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, None);

        let class = ceos.eos(None);
        assert_eq!(
            class,
            Class::Http(SuccessOrFailure::Success, StatusCode::BAD_REQUEST)
        );
    }

    #[test]
    fn http_response_status_server_error() {
        let rsp = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(())
            .unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, None);

        let class = ceos.eos(None);
        assert_eq!(
            class,
            Class::Http(SuccessOrFailure::Failure, StatusCode::INTERNAL_SERVER_ERROR)
        );
    }

    #[test]
    fn grpc_response_header_ok() {
        let rsp = Response::builder()
            .header("grpc-status", "0")
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, Some(Class::Grpc(SuccessOrFailure::Success, 0)));

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
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, Some(Class::Grpc(SuccessOrFailure::Failure, 2)));

        let class = ceos.eos(None);
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 2));
    }

    #[test]
    fn grpc_response_trailer_ok() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, None);

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());

        let class = ceos.eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Success, 0));
    }

    #[test]
    fn grpc_response_trailer_error() {
        let rsp = Response::builder().status(StatusCode::OK).body(()).unwrap();
        let crsp = ClassifyResponse {};
        let (ceos, class) = crsp.start(&rsp);
        assert_eq!(class, None);

        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "3".parse().unwrap());

        let class = ceos.eos(Some(&trailers));
        assert_eq!(class, Class::Grpc(SuccessOrFailure::Failure, 3));
    }
}
