use super::{propagation, Span, SpanSink};
use futures::{try_ready, Async, Future, Poll};
use std::collections::HashMap;
use std::time::SystemTime;
use tracing::{trace, warn};

pub struct ResponseFuture<F, S> {
    trace: Option<(Span, S)>,
    inner: F,
}

#[derive(Clone, Debug)]
pub struct Layer<S> {
    sink: Option<S>,
}

#[derive(Clone, Debug)]
pub struct Stack<M, S> {
    inner: M,
    sink: Option<S>,
}

pub struct MakeFuture<F, S> {
    inner: F,
    sink: Option<S>,
}

#[derive(Clone, Debug)]
pub struct Service<Svc, S> {
    inner: Svc,
    sink: Option<S>,
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
pub fn layer<S>(sink: Option<S>) -> Layer<S> {
    Layer { sink }
}

// === impl Layer ===

impl<M, S> tower::layer::Layer<M> for Layer<S>
where
    S: Clone,
{
    type Service = Stack<M, S>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            sink: self.sink.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M, S> tower::Service<T> for Stack<M, S>
where
    M: tower::Service<T>,
    S: Clone,
{
    type Response = Service<M::Response, S>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, S>;

    fn poll_ready(&mut self) -> Poll<(), M::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);

        MakeFuture {
            inner,
            sink: self.sink.clone(),
        }
    }
}

// === impl MakeFuture ===

impl<F: Future, S> Future for MakeFuture<F, S> {
    type Item = Service<F::Item, S>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let sink = self.sink.take();
        Ok(Async::Ready(Service { inner, sink }))
    }
}

// === impl Service ===

impl<Svc, B1, B2, S> tower::Service<http::Request<B1>> for Service<Svc, S>
where
    Svc: tower::Service<http::Request<B1>, Response = http::Response<B2>>,
    S: SpanSink + Clone,
{
    type Response = Svc::Response;
    type Error = Svc::Error;
    type Future = ResponseFuture<Svc::Future, S>;

    fn poll_ready(&mut self) -> Poll<(), Svc::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut request: http::Request<B1>) -> Self::Future {
        let sink = match &self.sink {
            Some(sink) => sink.clone(),
            None => {
                return ResponseFuture {
                    trace: None,
                    inner: self.inner.call(request),
                }
            }
        };

        let trace_context = propagation::unpack_trace_context(&request);
        let mut span = None;

        if let Some(context) = trace_context {
            trace!(message = "got trace context", ?context);
            let span_id = propagation::increment_span_id(&mut request, &context);
            // If we plan to sample this span, we need to record span metadata
            // from the request before dispatching it to inner.
            if context.is_sampled() {
                trace!(message = "span will be sampled", ?span_id);
                let path = request
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_owned());
                let mut labels = HashMap::new();
                request_labels(&mut labels, &request);
                span = Some(Span {
                    trace_id: context.trace_id,
                    span_id,
                    parent_id: context.parent_id,
                    span_name: path.unwrap_or_default(),
                    start: SystemTime::now(),
                    // End time will be updated when the span completes.
                    end: SystemTime::UNIX_EPOCH,
                    labels,
                });
            }
        }

        let f = self.inner.call(request);

        ResponseFuture {
            trace: span.map(|span| (span, sink)),
            inner: f,
        }
    }
}

// === impl SpanFuture ===

impl<F, S, B2> Future for ResponseFuture<F, S>
where
    F: Future<Item = http::Response<B2>>,
    S: SpanSink,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        if let Some((mut span, mut sink)) = self.trace.take() {
            span.end = SystemTime::now();
            response_labels(&mut span.labels, &inner);
            trace!(message = "emitting span", ?span);
            if let Err(error) = sink.try_send(span) {
                warn!(message = "span dropped", %error);
            }
        }
        Ok(Async::Ready(inner))
    }
}

fn request_labels<Body>(labels: &mut HashMap<String, String>, req: &http::Request<Body>) {
    labels.insert("http.method".to_string(), format!("{}", req.method()));
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_default();
    labels.insert("http.path".to_string(), path);
    if let Some(authority) = req.uri().authority_part() {
        labels.insert("http.authority".to_string(), authority.as_str().to_string());
    }
    if let Some(host) = req.headers().get("host") {
        if let Ok(host) = host.to_str() {
            labels.insert("http.host".to_string(), host.to_string());
        }
    }
}

fn response_labels<Body>(labels: &mut HashMap<String, String>, rsp: &http::Response<Body>) {
    labels.insert(
        "http.status_code".to_string(),
        rsp.status().as_str().to_string(),
    );
}
