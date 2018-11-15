use futures::Poll;
use http;

use svc;

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
    fn start<B>(self, headers: &http::Response<B>) -> Self::ClassifyEos;

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

// Used for stack targets that can produce a `Classify` implementation.
pub trait CanClassify {
    type Classify: Classify;

    fn classify(&self) -> Self::Classify;
}

#[derive(Debug, Clone)]
pub struct Layer();

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
}

#[derive(Clone, Debug)]
pub struct Service<C, S> {
    classify: C,
    inner: S,
}

pub fn layer() -> Layer {
    Layer()
}

impl<T, M> svc::Layer<T, T, M> for Layer
where
    T: CanClassify,
    M: svc::Stack<T>,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack { inner }
    }
}

impl<T, M> svc::Stack<T> for Stack<M>
where
    T: CanClassify,
    M: svc::Stack<T>,
{
    type Value = Service<T::Classify, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(target)?;
        let classify = target.classify();
        Ok(Service { classify, inner })
    }
}

impl<C, S, A, B> svc::Service<http::Request<A>> for Service<C, S>
where
    C: Classify,
    S: svc::Service<http::Request<A>, Response = http::Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<A>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        let _ = req.extensions_mut().insert(classify_rsp);

        self.inner.call(req)
    }
}
