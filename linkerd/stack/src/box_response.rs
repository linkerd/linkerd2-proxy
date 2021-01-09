use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A middleware that boxes *just* the response future from an inner service,
/// without erasing the service's type (and its trait impls, such as `Clone`).
///
/// This is primarily useful when a service's `Future` type is not `Unpin` and
/// must be boxed.
#[derive(Clone, Debug)]
pub struct BoxResponse<T>(T);

impl<T> BoxResponse<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }

    pub fn layer() -> impl tower::Layer<T, Service = Self> {
        crate::layer::mk(Self::new)
    }
}

impl<T, R> tower::Service<R> for BoxResponse<T>
where
    T: tower::Service<R>,
    T::Future: Send + 'static,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: R) -> Self::Future {
        Box::pin(self.0.call(req))
    }
}
