use super::{propagation, Span};
use futures::{try_ready, Async, Future, Poll};
use std::time::SystemTime;
use tokio::sync::mpsc;
use tracing::{trace, warn};

pub struct SpanFuture<F> {
    span: Option<Span>,
    inner: F,
    sender: mpsc::Sender<Span>,
}

#[derive(Clone, Debug)]
pub struct Layer {
    // TODO: Replace mpsc::Sender with a trait so that we can accept other
    // implementations.
    sender: mpsc::Sender<Span>,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
    sender: mpsc::Sender<Span>,
}

pub struct MakeFuture<F> {
    inner: F,
    sender: Option<mpsc::Sender<Span>>,
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    inner: S,
    sender: mpsc::Sender<Span>,
}

/// A layer that adds distributed tracing instrumentation.
///
/// This layer reads the `traceparent` HTTP header from the request.  If this
/// header is absent, the request is fowarded unmodified.  If the header is
/// present, a new span will be started in the current trace by creating a new
/// random span id setting it into the `traceparent` header before forwarding
/// the request.  If the sampled bit of the header was set, we emit metadata
/// about the span to the returned channel when the span is complete, i.e. when
/// we receive the response.
pub fn layer(sender: mpsc::Sender<Span>) -> Layer {
    Layer { sender }
}

// === impl Layer ===

impl<M> tower::layer::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            sender: self.sender.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M> tower::Service<T> for Stack<M>
where
    M: tower::Service<T>,
{
    type Response = Service<M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);

        MakeFuture {
            inner,
            sender: Some(self.sender.clone()),
        }
    }
}

// === impl MakeFuture ===

impl<F: Future> Future for MakeFuture<F> {
    type Item = Service<F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let sender = self.sender.take().expect("poll called after ready");
        Ok(Async::Ready(Service { inner, sender }))
    }
}

// === impl Service ===

impl<S, B> tower::Service<http::Request<B>> for Service<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::Either<S::Future, SpanFuture<S::Future>>;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let mut trace_context = propagation::unpack_trace_context(&request);
        let mut path = None;

        if let Some(ref mut context) = trace_context {
            trace!(message = "got trace context", ?context);
            propagation::increment_span_id(&mut request, context);
            // If we plan to sample this span, we need to copy the path from
            // the request before dispatching it to inner.
            if context.is_sampled() {
                path = Some(request.uri().path().to_string());
            }
        }

        let f = self.inner.call(request);

        if let Some(propagation::TraceContext {
            propagation: _,
            version: _,
            trace_id,
            parent_id,
            flags,
            span_id: Some(span_id),
        }) = trace_context
        {
            if flags.is_sampled() {
                trace!(message = "span will be sampled", ?span_id);
                let span = Span {
                    trace_id,
                    span_id,
                    parent_id,
                    span_name: path.unwrap_or_else(String::new),
                    start: SystemTime::now(),
                    // End time will be updated when the span completes.
                    end: SystemTime::now(),
                };
                let span_fut = SpanFuture {
                    span: Some(span),
                    inner: f,
                    sender: self.sender.clone(),
                };
                return futures::future::Either::B(span_fut);
            }
        }
        futures::future::Either::A(f)
    }
}

// === impl SpanFuture ===

impl<F: Future> Future for SpanFuture<F> {
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let mut span = self.span.take().expect("polled after ready");
        span.end = SystemTime::now();
        trace!(message = "emitting span", ?span);
        self.sender.try_send(span).unwrap_or_else(|_| {
            warn!("span dropped due to backpressure");
        });
        Ok(Async::Ready(inner))
    }
}
