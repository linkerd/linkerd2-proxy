use futures::Poll;
use tower;
use tracing::Span;
use tracing_futures::{Instrument, Instrumented};

pub trait InstrumentRequest<T> {
    fn span(&self, req: &T) -> Span;
}

#[derive(Clone, Debug)]
pub struct Service<I, S> {
    instrument: I,
    service: S,
}

impl<I, S> Service<I, S> {
    pub fn new<T>(instrument: I, service: S) -> Self
    where
        Self: tower::Service<T>,
    {
        Self {
            instrument,
            service,
        }
    }
}

impl<T, I, S> tower::Service<T> for Service<I, S>
where
    I: InstrumentRequest<T>,
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    #[inline]
    fn call(&mut self, req: T) -> Self::Future {
        let span = self.instrument.span(&req);
        self.service.call(req).instrument(span)
    }
}

impl<T, F> InstrumentRequest<T> for F
where
    F: Fn(&T) -> Span,
{
    fn span(&self, req: &T) -> Span {
        (self)(req)
    }
}
