#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod service;

pub use self::service::NewClassify;
use linkerd_error::Error;

/// Determines how a request's response should be classified.
pub trait Classify {
    type Class;
    type ClassifyEos: ClassifyEos<Class = Self::Class>;

    /// Classifies responses.
    ///
    /// Instances are intended to be used as an `http::Extension` that may be
    /// cloned to inner stack layers. Cloned instances are **not** intended to
    /// share state. Each clone should maintain its own internal state.
    type ClassifyResponse: ClassifyResponse<Class = Self::Class, ClassifyEos = Self::ClassifyEos>
        + Clone
        + Send
        + Sync
        + 'static;

    fn classify<B>(&self, req: &http::Request<B>) -> Self::ClassifyResponse;
}

/// Classifies a single response.
pub trait ClassifyResponse {
    /// A response classification.
    type Class;
    type ClassifyEos: ClassifyEos<Class = Self::Class>;

    /// Produce a stream classifier for this response.
    fn start<B>(self, headers: &http::Response<B>) -> Self::ClassifyEos;

    /// Classifies the given error.
    fn error(self, error: &Error) -> Self::Class;
}

pub trait ClassifyEos {
    type Class;

    /// Update the classifier with an EOS.
    ///
    /// Because trailers indicate an EOS, a classification must be returned.
    fn eos(self, trailers: Option<&http::HeaderMap>) -> Self::Class;

    /// Update the classifier with an underlying error.
    ///
    /// Because errors indicate an end-of-stream, a classification must be
    /// returned.
    fn error(self, error: &Error) -> Self::Class;
}

// Used for stack targets that can produce a `Classify` implementation.
pub trait CanClassify {
    type Classify: Classify;

    fn classify(&self) -> Self::Classify;
}
