#![deny(warnings, rust_2018_idioms)]
use futures::{ready, Stream};
use http_body::Body as HttpBody;
use linkerd2_error::Error;
use linkerd2_stack::NewService;
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opencensus_proto::trace::v1::Span;
use pin_project::pin_project;
use std::convert::TryInto;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tonic::{
    self as grpc,
    body::{Body as GrpcBody, BoxBody},
    client::GrpcService,
    Streaming,
};
use tracing::trace;

pub mod metrics;

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
#[pin_project]
pub struct SpanExporter<T, S> {
    client: T,
    node: Node,
    state: State,
    #[pin]
    spans: S,
    max_batch_size: usize,
    metrics: Registry,
}

enum State {
    Idle,
    Sending {
        sender: mpsc::Sender<ExportTraceServiceRequest>,
        // Node data should only be sent on the first message of a streaming
        // request.
        node: Option<Node>,
        // We hold the response future, but never poll it.
        _rsp: Pin<
            Box<
                dyn Future<
                        Output = Result<
                            grpc::Response<Streaming<ExportTraceServiceResponse>>,
                            grpc::Status,
                        >,
                    > + Send
                    + 'static,
            >,
        >,
        metrics: Registry,
    },
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: NewService<()>,
    T::Service: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
{
    const DEFAULT_MAX_BATCH_SIZE: usize = 100;

    pub fn new(client: T, node: Node, spans: S, metrics: Registry) -> Self {
        Self {
            client,
            node,
            spans,
            state: State::Idle,
            max_batch_size: Self::DEFAULT_MAX_BATCH_SIZE,
            metrics,
        }
    }

    fn do_send(
        spans: Vec<Span>,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        node: &mut Option<Node>,
        metrics: &mut Registry,
    ) -> Result<(), ()> {
        if spans.is_empty() {
            return Ok(());
        }

        if let Ok(num_spans) = spans.len().try_into() {
            metrics.send(num_spans);
        }
        let req = ExportTraceServiceRequest {
            spans,
            node: node.take(),
            resource: None,
        };
        trace!(message = "Transmitting", ?req);
        sender.try_send(req).map_err(|_| ())
    }

    /// Attempt to read spans from the spans stream and write it to
    /// the export streaming request.  Returns NotReady there isn't room in
    /// the streaming request or if there isn't a span available in the spans
    /// stream.
    ///
    /// Returns Ready if the receiver is fully consumed and may no longer be
    /// used.
    ///
    /// An error is returned if the sender fails and must be discarded.
    ///
    /// Otherwise NotReady is returned.
    fn poll_send_spans(
        mut receiver: Pin<&mut S>,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        node: &mut Option<Node>,
        max_batch_size: usize,
        metrics: &mut Registry,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ()>> {
        ready!(sender.poll_ready(cx)).map_err(|_| ())?;

        let mut spans = Vec::new();
        loop {
            match receiver.as_mut().poll_next(cx) {
                Poll::Pending => {
                    // If any spans have been collected send them, potentially consuming `node.
                    Self::do_send(spans, sender, node, metrics)?;
                    return Poll::Pending;
                }
                Poll::Ready(Some(span)) => {
                    spans.push(span);
                    if spans.len() == max_batch_size {
                        Self::do_send(spans, sender, node, metrics)?;
                        // Because we've voluntarily stopped work due to a batch
                        // size limitation, notify the task to be polled again
                        // immediately.
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => {
                    let _ = Self::do_send(spans, sender, node, metrics);
                    // The span receiver stream completed, so signal completion.
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl<T, S, Svc> Future for SpanExporter<T, S>
where
    T: NewService<(), Service = Svc>,
    Svc: GrpcService<BoxBody> + Send + 'static,
    S: Stream<Item = Span>,
    Svc::Error: Into<Error> + Send,
    Svc::ResponseBody: Send + 'static,
    <Svc::ResponseBody as GrpcBody>::Data: Send,
    <Svc::ResponseBody as HttpBody>::Error: Into<Error> + Send,
    Svc::Future: Send,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let this = self.as_mut().project();
            *this.state = match this.state {
                State::Idle => {
                    let (request_tx, request_rx) = mpsc::channel(1);
                    let mut svc = TraceServiceClient::new(this.client.new_service(()));
                    let req = grpc::Request::new(request_rx);
                    trace!("Establishing new TraceService::export request");
                    this.metrics.start_stream();
                    let _rsp = Box::pin(async move { svc.export(req).await });
                    State::Sending {
                        sender: request_tx,
                        node: Some(this.node.clone()),
                        _rsp,
                        metrics: this.metrics.clone(),
                    }
                }
                State::Sending {
                    ref mut sender,
                    ref mut node,
                    ref mut metrics,
                    ..
                } => {
                    match ready!(Self::poll_send_spans(
                        this.spans,
                        sender,
                        node,
                        *this.max_batch_size,
                        metrics,
                        cx,
                    )) {
                        Ok(()) => return Poll::Ready(()),
                        Err(()) => State::Idle,
                    }
                }
            };
        }
    }
}

struct IntoService<T>(T);

impl<T, R> tower::Service<http::Request<R>> for IntoService<T>
where
    T: GrpcService<R>,
{
    type Response = http::Response<T::ResponseBody>;
    type Future = T::Future;
    type Error = T::Error;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<R>) -> Self::Future {
        self.0.call(req)
    }
}
