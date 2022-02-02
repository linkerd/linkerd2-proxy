#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

use linkerd_stack::{layer, NewService, Proxy};
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
#[derive(Debug)]
pub struct Instrument<T, G: GetSpan<T>, S> {
    target: T,
    /// When this is a `Service` (and not a `Proxy`), we consider the `poll_ready`
    /// calls that drive the service to readiness and the `call` future that
    /// consumes that readiness to be part of one logical span (so, for example,
    /// we track time waiting for readiness as part of the request span's idle
    /// time).
    ///
    /// Therefore, we hang onto one instance of the span that's created when we
    /// are first polled after having been called, and take that span instance
    /// in `call`.
    current_span: Option<Span>,
    get_span: G,
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

    fn new_service(&self, target: T) -> Self::Service {
        let _span = self.get_span.get_span(&target).entered();
        trace!("new");
        let inner = self.inner.new_service(target.clone());
        Instrument {
            inner,
            target,
            current_span: None,
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
        // These need to be borrowed individually, or else the
        // `get_or_insert_with` closure will borrow *all* of `self` while
        // `self.current_span` is borrowed mutably... T_T
        let get_span = &self.get_span;
        let target = &self.target;
        let _enter = self
            .current_span
            .get_or_insert_with(|| get_span.get_span(target))
            .enter();

        let ready = self.inner.poll_ready(cx);
        match ready {
            Poll::Pending => trace!(ready = false, "service"),
            Poll::Ready(ref res) => trace!(ready = true, ok = res.is_ok(), "service"),
        }
        ready
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let span = self
            .current_span
            .take()
            // NOTE(eliza): if `current_span` is `None` here, we were called
            // before  being driven to readiness, which is invalid --- we're
            // permitted to panic here, so we could unwrap this. But, it's not
            // important...we can just make a new span, so I thought it was
            // better to err on the side of not panicking.
            .unwrap_or_else(|| self.get_span())
            .entered();

        trace!(?request, "service");
        self.inner.call(request).instrument(span.exit())
    }
}

impl<T, G, S> Clone for Instrument<T, G, S>
where
    T: Clone,
    G: Clone + GetSpan<T>,
    S: Clone,
{
    fn clone(&self) -> Self {
        // Manually implement `Clone` so that each clone of an instrumented
        // service has its own "current span" state, since each clone of the
        // inner service will have its own independent readiness state.
        Self {
            target: self.target.clone(),
            inner: self.inner.clone(),
            get_span: self.get_span.clone(),
            // If this is a `Service`, the clone will construct its own span
            // when it's first driven to readiness.
            current_span: None,
        }
    }
}

impl<T, G: GetSpan<T>, S> Drop for Instrument<T, G, S> {
    fn drop(&mut self) {
        let span = self.current_span.take().unwrap_or_else(|| self.get_span());
        trace!(parent: &span, "drop");
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
