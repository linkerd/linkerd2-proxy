use std::marker::PhantomData;
pub use tower::util::BoxService;

#[derive(Copy, Debug)]
pub struct BoxServiceLayer<R> {
    _p: PhantomData<fn(R)>,
}

impl<S, R> tower::Layer<S> for BoxServiceLayer<R>
where
    S: tower::Service<R> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Service = BoxService<R, S::Response, S::Error>;
    fn layer(&self, s: S) -> Self::Service {
        BoxService::new(s)
    }
}

impl<R> BoxServiceLayer<R> {
    pub fn new() -> Self {
        Self { _p: PhantomData }
    }
}

impl<R> Clone for BoxServiceLayer<R> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<R> Default for BoxServiceLayer<R> {
    fn default() -> Self {
        Self::new()
    }
}
