use crate::Buffer;
use linkerd_error::Error;
use std::time::Duration;
use tracing::instrument::Instrument;

pub struct SpawnBufferLayer<Req, Rsp> {
    capacity: usize,
    idle_timeout: Option<Duration>,
    _marker: std::marker::PhantomData<fn(Req) -> Rsp>,
}

impl<Req, Rsp> SpawnBufferLayer<Req, Rsp> {
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

impl<Req, Rsp> Clone for SpawnBufferLayer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            idle_timeout: self.idle_timeout,
            _marker: self._marker,
        }
    }
}

impl<Req, Rsp, S> tower::layer::Layer<S> for SpawnBufferLayer<Req, Rsp>
where
    Req: Send + 'static,
    Rsp: Send + 'static,
    S: tower::Service<Req, Response = Rsp> + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Future: Send + 'static,
{
    type Service = Buffer<Req, Rsp>;

    fn layer(&self, inner: S) -> Self::Service {
        let (buffer, dispatch) = crate::new(inner, self.capacity, self.idle_timeout);
        tokio::spawn(dispatch.in_current_span());
        buffer
    }
}
