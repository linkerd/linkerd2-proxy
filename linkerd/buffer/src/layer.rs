use crate::Buffer;
use futures::Future;
use linkerd2_error::Error;
use tracing_futures::Instrument;

pub struct SpawnBufferLayer<Req> {
    capacity: usize,
    _marker: std::marker::PhantomData<fn(Req)>,
}

impl<Req> SpawnBufferLayer<Req> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<Req> Clone for SpawnBufferLayer<Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            _marker: self._marker,
        }
    }
}

impl<Req, S> tower::layer::Layer<S> for SpawnBufferLayer<Req>
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Service = Buffer<Req, S::Future>;

    fn layer(&self, inner: S) -> Self::Service {
        let (buffer, dispatch) = crate::new(inner, self.capacity);
        tokio::spawn(dispatch.in_current_span().map_err(|n| match n {}));
        buffer
    }
}
