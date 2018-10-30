use http;

/// Determines how a request's response should be classified.
pub trait Classify {
    type Class;
    type Error;

    type ClassifyEos: ClassifyEos<Class = Self::Class, Error = Self::Error>;

    /// Classifies responses.
    ///
    /// Instances are intended to be used as an `http::Extension` that may be
    /// cloned to inner stack layers. Cloned instances are **not** intended to
    /// share state. Each clone should maintain its own internal state.
    type ClassifyResponse: ClassifyResponse<
            Class = Self::Class,
            Error = Self::Error,
            ClassifyEos = Self::ClassifyEos,
        >
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
    type Error;
    type ClassifyEos: ClassifyEos<Class = Self::Class, Error = Self::Error>;

    /// Produce a stream classifier for this response.
    ///
    /// If this is enough data to classify a response, a classification may be
    /// returned.
    fn start<B>(self, headers: &http::Response<B>) -> (Self::ClassifyEos, Option<Self::Class>);

    /// Classifies the given error.
    fn error(self, error: &Self::Error) -> Self::Class;
}

pub trait ClassifyEos {
    type Class;
    type Error;

    /// Update the classifier with an EOS.
    ///
    /// Because trailers indicate an EOS, a classification must be returned.
    fn eos(self, trailers: Option<&http::HeaderMap>) -> Self::Class;

    /// Update the classifier with an underlying error.
    ///
    /// Because errors indicate an end-of-stream, a classification must be
    /// returned.
    fn error(self, error: &Self::Error) -> Self::Class;
}
