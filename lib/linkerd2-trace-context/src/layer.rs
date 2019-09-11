use super::{propagation, Span, SpanSink};
use futures::{future::Either, try_ready, Async, Future, Poll};
use std::time::SystemTime;
use tracing::{trace, warn};

pub struct SpanFuture<Fut, Sink> {
    span: Option<Span>,
    inner: Fut,
    sink: Sink,
}

#[derive(Clone, Debug)]
pub struct Layer<Sink> {
    sink: Sink,
}

#[derive(Clone, Debug)]
pub struct Stack<Make, Sink> {
    inner: Make,
    sink: Sink,
}

pub struct MakeFuture<Fut, Sink> {
    inner: Fut,
    sink: Option<Sink>,
}

#[derive(Clone, Debug)]
pub struct Service<Svc, Sink> {
    inner: Svc,
    sink: Sink,
}

/// A layer that adds distributed tracing instrumentation.
///
/// This layer reads the `traceparent` HTTP header from the request.  If this
/// header is absent, the request is fowarded unmodified.  If the header is
/// present, a new span will be started in the current trace by creating a new
/// random span id setting it into the `traceparent` header before forwarding
/// the request.  If the sampled bit of the header was set, we emit metadata
/// about the span to the given SpanSink when the span is complete, i.e. when
/// we receive the response.
pub fn layer<Sink>(sink: Sink) -> Layer<Sink> {
    Layer { sink }
}

// === impl Layer ===

impl<Make, Sink> tower::layer::Layer<Make> for Layer<Sink>
where
    Sink: Clone,
{
    type Service = Stack<Make, Sink>;

    fn layer(&self, inner: Make) -> Self::Service {
        Stack {
            inner,
            sink: self.sink.clone(),
        }
    }
}

// === impl Stack ===

impl<Target, Make, Sink> tower::Service<Target> for Stack<Make, Sink>
where
    Make: tower::Service<Target>,
    Sink: Clone,
{
    type Response = Service<Make::Response, Sink>;
    type Error = Make::Error;
    type Future = MakeFuture<Make::Future, Sink>;

    fn poll_ready(&mut self) -> Poll<(), Make::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: Target) -> Self::Future {
        let inner = self.inner.call(target);

        MakeFuture {
            inner,
            sink: Some(self.sink.clone()),
        }
    }
}

// === impl MakeFuture ===

impl<Fut: Future, Sink> Future for MakeFuture<Fut, Sink> {
    type Item = Service<Fut::Item, Sink>;
    type Error = Fut::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let sink = self.sink.take().expect("poll called after ready");
        Ok(Async::Ready(Service { inner, sink }))
    }
}

// === impl Service ===

impl<Svc, Body, Sink> tower::Service<http::Request<Body>> for Service<Svc, Sink>
where
    Svc: tower::Service<http::Request<Body>>,
    Sink: SpanSink + Clone,
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = futures::future::Either<Svc::Future, SpanFuture<Svc::Future, Sink>>;

    fn poll_ready(&mut self) -> Poll<(), Svc::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
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
            trace_id,
            parent_id,
            flags,
            span_id: Some(span_id),
            ..
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
                    sink: self.sink.clone(),
                };
                return Either::B(span_fut);
            }
        }
        Either::A(f)
    }
}

// === impl SpanFuture ===

impl<Fut, Sink> Future for SpanFuture<Fut, Sink>
where
    Fut: Future,
    Sink: SpanSink,
{
    type Item = Fut::Item;
    type Error = Fut::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let mut span = self.span.take().expect("polled after ready");
        span.end = SystemTime::now();
        trace!(message = "emitting span", ?span);
        if self.sink.try_send(span).is_err() {
            warn!("span dropped due to backpressure");
        }
        Ok(Async::Ready(inner))
    }
}
