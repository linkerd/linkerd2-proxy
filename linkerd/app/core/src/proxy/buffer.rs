use crate::svc;
use linkerd2_error::Error;
use std::marker::PhantomData;
pub use tower::buffer::Buffer;

#[derive(Debug)]
pub struct Layer<Req> {
    capacity: usize,
    _marker: PhantomData<fn(Req)>,
}

// === impl Layer ===

impl<Req> Layer<Req> {
    pub fn new(capacity: usize) -> Self
    where
        Req: Send + 'static,
    {
        Layer {
            capacity,
            _marker: PhantomData,
        }
    }
}

impl<Req> Clone for Layer<Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            _marker: PhantomData,
        }
    }
}

impl<S, Req> svc::Layer<S> for Layer<Req>
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error> + Send + Sync,
    S::Future: Send,
{
    type Service = Buffer<S, Req>;

    fn layer(&self, inner: S) -> Self::Service {
        Buffer::new(inner, self.capacity)
    }
}
