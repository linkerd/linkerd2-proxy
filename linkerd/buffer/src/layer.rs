use crate::Buffer;
use futures::Future;
use linkerd2_error::Error;
use std::time::Duration;
use tracing_futures::Instrument;

pub struct SpawnBufferLayer<Req> {
    capacity: usize,
    idle_timeout: Option<Duration>,
    _marker: std::marker::PhantomData<fn(Req)>,
}

impl<Req> SpawnBufferLayer<Req> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            idle_timeout: None,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }
}

impl<Req> Clone for SpawnBufferLayer<Req> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            idle_timeout: self.idle_timeout,
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
        let (buffer, dispatch) = crate::new(inner, self.capacity, self.idle_timeout);
        tokio::spawn(dispatch.in_current_span().map_err(|n| match n {}));
        buffer
    }
}
