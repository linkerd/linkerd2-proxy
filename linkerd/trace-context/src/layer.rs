use crate::{propagation, Span, SpanSink};
use futures::{prelude::*, ready};
use linkerd2_error::Error;
use linkerd2_stack::{layer, Either};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::SystemTime,
};
use tracing::{debug, info, trace};

/// A layer that adds distributed tracing instrumentation.
///
/// This layer reads the `traceparent` HTTP header from the request. If this
/// header is absent, the request is fowarded unmodified.  If the header is
/// present, a new span will be started in the current trace by creating a new
/// random span id setting it into the `traceparent` header before forwarding
/// the request. If the sampled bit of the header was set, we emit metadata
/// about the span to the given SpanSink when the span is complete, i.e. when
/// we receive the response.
#[derive(Clone, Debug)]
pub struct TraceContext<K, S> {
    inner: S,
    sink: Option<K>,
}

// === impl TraceContext ===

impl<K: Clone, S> TraceContext<K, S> {
    pub fn layer(sink: Option<K>) -> impl layer::Layer<S, Service = TraceContext<K, S>> + Clone {
        layer::mk(move |inner| TraceContext {
            inner,
            sink: sink.clone(),
        })
    }

    fn request_labels<B>(req: &http::Request<B>) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert("http.method".to_string(), format!("{}", req.method()));
        let path = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_owned())
            .unwrap_or_default();
        labels.insert("http.path".to_string(), path);
        if let Some(authority) = req.uri().authority() {
            labels.insert("http.authority".to_string(), authority.as_str().to_string());
        }
        if let Some(host) = req.headers().get("host") {
            if let Ok(host) = host.to_str() {
                labels.insert("http.host".to_string(), host.to_string());
            }
        }
        labels
    }

    fn add_response_labels<B>(
        mut labels: HashMap<String, String>,
        rsp: &http::Response<B>,
    ) -> HashMap<String, String> {
        labels.insert(
            "http.status_code".to_string(),
            rsp.status().as_str().to_string(),
        );
        labels
    }
}

impl<K, S, ReqB, RspB> tower::Service<http::Request<ReqB>> for TraceContext<K, S>
where
    S: tower::Service<http::Request<ReqB>, Response = http::Response<RspB>>,
    S::Error: Into<Error> + Send,
    S::Future: Send + 'static,
    K: SpanSink + Clone + Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Either<
        S::Future,
        Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(ready!(self.inner.poll_ready(cx)).map_err(Into::into))
    }

    fn call(&mut self, mut req: http::Request<ReqB>) -> Self::Future {
        if let Some(sink) = self.sink.as_ref() {
            if let Some(context) = propagation::unpack_trace_context(&req) {
                // Update the trace ID if the request set one and the proxy is configured to emit
                // spans.
                let span_id = propagation::increment_span_id(&mut req, &context);
                debug!(?span_id, sampled = context.is_sampled());

                if context.is_sampled() {
                    // If the request has been marked for sampling, record its metadata.
                    let start = SystemTime::now();
                    let req_labels = Self::request_labels(&req);
                    let mut sink = sink.clone();
                    let span_name = req
                        .uri()
                        .path_and_query()
                        .map(|pq| pq.as_str().to_owned())
                        .unwrap_or_default();
                    return Either::B(Box::pin(self.inner.call(req).map_ok(move |rsp| {
                        // Emit the completed span with the response metadtata.
                        let span = Span {
                            span_id,
                            trace_id: context.trace_id,
                            parent_id: context.parent_id,
                            span_name,
                            start,
                            end: SystemTime::now(),
                            labels: Self::add_response_labels(req_labels, &rsp),
                        };
                        trace!(?span);
                        if let Err(error) = sink.try_send(span) {
                            info!(%error, "Span dropped");
                        }
                        rsp
                    })));
                }
            }
        }

        // If there's no tracing to be done, just pass on the request to the inner service.
        Either::A(self.inner.call(req))
    }
}
