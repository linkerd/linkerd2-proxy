use futures::TryFuture;
use linkerd_stack::{NewService, Proxy};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{trace, Span};
use tracing::instrument::{Instrument as _, Instrumented};

/// A strategy for creating spans based on a service's target.
pub trait GetSpan<T> {
    fn get_span(&self, target: &T) -> tracing::Span;
}

/// A middleware that instruments tracing for stacks.
#[derive(Clone, Debug)]
pub struct InstrumentMakeLayer<G> {
    get_span: G,
}

/// Instruments a `MakeService` or `NewService` stack.
#[derive(Clone, Debug)]
pub struct InstrumentMake<G, M> {
    get_span: G,
    make: M,
}

/// Instruments a service produced by `InstrumentMake`.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Instrument<S> {
    span: Span,
    #[pin]
    inner: S,
}

// === impl InstrumentMakeLayer ===

impl<G> InstrumentMakeLayer<G> {
    pub fn new(get_span: G) -> Self {
        Self { get_span }
    }
}

impl InstrumentMakeLayer<()> {
    pub fn from_target() -> Self {
        Self::new(())
    }
}

impl<G: Clone, M> tower::layer::Layer<M> for InstrumentMakeLayer<G> {
    type Service = InstrumentMake<G, M>;

    fn layer(&self, make: M) -> Self::Service {
        Self::Service {
            make,
            get_span: self.get_span.clone(),
        }
    }
}

// === impl InstrumentMake ===

impl<T, G, N> NewService<T> for InstrumentMake<G, N>
where
    G: GetSpan<T>,
    N: NewService<T>,
{
    type Service = Instrument<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let span = self.get_span.get_span(&target);
        let inner = span.in_scope(move || {
            trace!("new");
            self.make.new_service(target)
        });
        Instrument { inner, span }
    }
}

impl<T, G, M> tower::Service<T> for InstrumentMake<G, M>
where
    G: GetSpan<T>,
    M: tower::Service<T>,
{
    type Response = Instrument<M::Response>;
    type Error = M::Error;
    type Future = Instrument<M::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self.make.poll_ready(cx);
        match ready {
            Poll::Pending => trace!(ready = false, "make"),
            Poll::Ready(ref res) => trace!(ready = true, ok = res.is_ok(), "make"),
        }
        ready
    }

    fn call(&mut self, target: T) -> Self::Future {
        let span = self.get_span.get_span(&target);
        let inner = span.in_scope(|| {
            trace!("make");
            self.make.call(target)
        });
        Instrument { inner, span }
    }
}

// === impl Instrument ===

impl<F> Future for Instrument<F>
where
    F: TryFuture,
{
    type Output = Result<Instrument<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _enter = this.span.enter();

        trace!("making");
        match this.inner.try_poll(cx)? {
            Poll::Pending => {
                trace!(ready = false);
                Poll::Pending
            }
            Poll::Ready(inner) => {
                trace!(ready = true);
                let svc = Instrument {
                    inner,
                    span: this.span.clone(),
                };
                Poll::Ready(Ok(svc))
            }
        }
    }
}

impl<Req, S, P> Proxy<Req, S> for Instrument<P>
where
    Req: std::fmt::Debug,
    P: Proxy<Req, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = Instrumented<P::Future>;

    fn proxy(&self, svc: &mut S, request: Req) -> Self::Future {
        let _enter = self.span.enter();
        trace!(?request, "proxy");
        self.inner.proxy(svc, request).instrument(self.span.clone())
    }
}

impl<Req, S> tower::Service<Req> for Instrument<S>
where
    Req: std::fmt::Debug,
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Instrumented<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let _enter = self.span.enter();

        let ready = self.inner.poll_ready(cx);
        match ready {
            Poll::Pending => trace!(ready = false, "service"),
            Poll::Ready(ref res) => trace!(ready = true, ok = res.is_ok(), "service"),
        }
        ready
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let _enter = self.span.enter();

        trace!(?request, "service");
        self.inner.call(request).instrument(self.span.clone())
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

impl<T> GetSpan<T> for tracing::Span {
    fn get_span(&self, _: &T) -> tracing::Span {
        self.clone()
    }
}
