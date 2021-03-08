use linkerd_stack::{layer, NewService, Proxy};
use pin_project::pin_project;
use std::task::{Context, Poll};
use tracing::instrument::{Instrument as _, Instrumented};
use tracing::{trace, Span};

/// A strategy for creating spans based on a service's target.
pub trait GetSpan<T> {
    fn get_span(&self, target: &T) -> tracing::Span;
}

#[derive(Clone, Debug)]
pub struct NewInstrumentLayer<G> {
    get_span: G,
}

/// Instruments a `MakeService` or `NewService` stack.
#[derive(Clone, Debug)]
pub struct NewInstrument<G, N> {
    get_span: G,
    inner: N,
}

/// Instruments a service produced by `NewInstrument`.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Instrument<T, G, S> {
    target: T,
    get_span: G,
    #[pin]
    inner: S,
}

// === impl NewInstrumentLayer ===

impl<G> NewInstrumentLayer<G> {
    pub fn new(get_span: G) -> Self {
        Self { get_span }
    }
}

impl NewInstrumentLayer<()> {
    pub fn from_target() -> Self {
        Self::new(())
    }
}

impl<G: Clone, N> layer::Layer<N> for NewInstrumentLayer<G> {
    type Service = NewInstrument<G, N>;

    fn layer(&self, inner: N) -> Self::Service {
        NewInstrument {
            inner,
            get_span: self.get_span.clone(),
        }
    }
}

// === impl NewInstrument ===

impl<G: Clone, N> NewInstrument<G, N> {
    pub fn layer(get_span: G) -> NewInstrumentLayer<G> {
        NewInstrumentLayer::new(get_span)
    }
}

impl<T, G, N> NewService<T> for NewInstrument<G, N>
where
    T: Clone,
    G: GetSpan<T> + Clone,
    N: NewService<T>,
{
    type Service = Instrument<T, G, N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let _span = self.get_span.get_span(&target).entered();
        trace!("new");
        let inner = self.inner.new_service(target.clone());
        Instrument {
            inner,
            target,
            get_span: self.get_span.clone(),
        }
    }
}

// === impl Instrument ===

impl<T, G: GetSpan<T>, S> Instrument<T, G, S> {
    #[inline]
    fn get_span(&self) -> Span {
        self.get_span.get_span(&self.target)
    }
}

impl<Req, S, T, G, P> Proxy<Req, S> for Instrument<T, G, P>
where
    Req: std::fmt::Debug,
    G: GetSpan<T>,
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = Instrumented<P::Future>;

    fn proxy(&self, svc: &mut S, request: Req) -> Self::Future {
        let span = self.get_span().entered();
        trace!(?request, "proxy");
        self.inner.proxy(svc, request).instrument(span.exit())
    }
}

impl<Req, T, G, S> tower::Service<Req> for Instrument<T, G, S>
where
    Req: std::fmt::Debug,
    G: GetSpan<T>,
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _span = self.get_span().entered();

        let ready = self.inner.poll_ready(cx);
        match ready {
            Poll::Pending => trace!(ready = false, "service"),
            Poll::Ready(ref res) => trace!(ready = true, ok = res.is_ok(), "service"),
        }
        ready
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let span = self.get_span().entered();

        trace!(?request, "service");
        self.inner.call(request).instrument(span.exit())
    }
}

// === impl GetSpan ===

impl<T, F> GetSpan<T> for F
where
    F: Fn(&T) -> tracing::Span,
{
    fn get_span(&self, target: &T) -> tracing::Span {
        (self)(target)
    }
}

impl<T: GetSpan<()>> GetSpan<T> for () {
    fn get_span(&self, t: &T) -> tracing::Span {
        t.get_span(&())
    }
}
