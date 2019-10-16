#![deny(warnings, rust_2018_idioms)]

use futures::{task, try_ready, Async, Future, Poll, Stream};
use linkerd2_error::Error;
use metrics::Registry;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    client::TraceService, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opencensus_proto::trace::v1::Span;
use std::convert::TryInto;
use tokio::sync::mpsc;
use tower_grpc::{
    self as grpc, client::streaming::ResponseFuture, generic::client::GrpcService, BoxBody,
};
use tracing::trace;

pub mod metrics;

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
pub struct SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
{
    client: T,
    node: Node,
    state: State<T>,
    spans: S,
    max_batch_size: usize,
    metrics: Registry,
}

enum State<T: GrpcService<BoxBody>> {
    Idle,
    Sending {
        sender: mpsc::Sender<ExportTraceServiceRequest>,
        // Node data should only be sent on the first message of a streaming
        // request.
        node: Option<Node>,
        // We hold the response future, but never poll it.
        _rsp: ResponseFuture<ExportTraceServiceResponse, T::Future>,
        metrics: Registry,
    },
}

enum StreamError<E> {
    Receiver(E),
    SenderLost,
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
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
    ) -> Result<(), StreamError<S::Error>> {
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
        sender.try_send(req).map_err(|_| StreamError::SenderLost)
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
        receiver: &mut S,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        node: &mut Option<Node>,
        max_batch_size: usize,
        metrics: &mut Registry,
    ) -> Poll<(), StreamError<S::Error>> {
        try_ready!(sender.poll_ready().map_err(|_| StreamError::SenderLost));

        let mut spans = Vec::new();
        loop {
            match receiver.poll() {
                Ok(Async::NotReady) => {
                    // If any spans have been collected send them, potentially consuming `node.
                    Self::do_send(spans, sender, node, metrics)?;
                    return Ok(Async::NotReady);
                }
                Ok(Async::Ready(Some(span))) => {
                    spans.push(span);
                    if spans.len() == max_batch_size {
                        Self::do_send(spans, sender, node, metrics)?;
                        // Because we've voluntarily stopped work due to a batch
                        // size limitation, notify the task to be polled again
                        // immediately.
                        task::current().notify();
                        return Ok(Async::NotReady);
                    }
                }
                Ok(Async::Ready(None)) => {
                    let _ = Self::do_send(spans, sender, node, metrics);
                    // The span receiver stream completed, so signal completion.
                    return Ok(Async::Ready(()));
                }
                Err(e) => {
                    let _ = Self::do_send(spans, sender, node, metrics);
                    return Err(StreamError::Receiver(e));
                }
            }
        }
    }
}

impl<T, S> Future for SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
    S::Error: Into<Error>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match self.state {
                State::Idle => {
                    let (request_tx, request_rx) = mpsc::channel(1);
                    let mut svc = TraceService::new(self.client.as_service());
                    try_ready!(svc.poll_ready());
                    let req = grpc::Request::new(
                        request_rx
                            .map_err(|_| grpc::Status::new(grpc::Code::Cancelled, "cancelled")),
                    );
                    trace!("Establishing new TraceService::export request");
                    self.metrics.start_stream();
                    let _rsp = svc.export(req);
                    State::Sending {
                        sender: request_tx,
                        node: Some(self.node.clone()),
                        _rsp,
                        metrics: self.metrics.clone(),
                    }
                }
                State::Sending {
                    ref mut sender,
                    ref mut node,
                    ref mut metrics,
                    ..
                } => {
                    let mut sender = sender.clone();
                    match Self::poll_send_spans(
                        &mut self.spans,
                        &mut sender,
                        node,
                        self.max_batch_size,
                        metrics,
                    ) {
                        Ok(ready) => return Ok(ready),
                        Err(StreamError::Receiver(e)) => return Err(e.into()),
                        Err(StreamError::SenderLost) => State::Idle,
                    }
                }
            };
        }
    }
}
