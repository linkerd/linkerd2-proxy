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
        .map(|grpc_status| if grpc_status == 0 {
            Class::Grpc(SuccessOrFailure::Success, grpc_status)
        } else {
            Class::Grpc(SuccessOrFailure::Failure, grpc_status)
        })
}
