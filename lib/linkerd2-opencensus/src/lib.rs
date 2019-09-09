use futures::{task, try_ready, Async, Future, Poll, Stream};
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    client::TraceService, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opencensus_proto::trace::v1::Span;
use tokio::sync::mpsc;
use tower_grpc::{
    self as grpc, client::streaming::ResponseFuture, generic::client::GrpcService, BoxBody,
};
use tracing::{trace, warn};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

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
    },
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
    S::Error: Into<Error>,
{
    const DEFAULT_MAX_BATCH_SIZE: usize = 100;

    pub fn new(client: T, node: Node, spans: S) -> Self {
        Self {
            client,
            node,
            spans,
            state: State::Idle,
            max_batch_size: Self::DEFAULT_MAX_BATCH_SIZE,
        }
    }

    fn do_send(
        spans: Vec<Span>,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        node: &mut Option<Node>,
    ) -> Result<(), ()> {
        if spans.is_empty() {
            return Ok(());
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
        receiver: &mut S,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        node: &mut Option<Node>,
        max_batch_size: usize,
    ) -> Poll<(), ()> {
        try_ready!(sender.poll_ready().map_err(|_| ()));

        let mut spans = Vec::new();
        loop {
            match receiver.poll() {
                Ok(Async::NotReady) => {
                    // If any spans have been collected send them, potentially consuming `node.
                    Self::do_send(spans, sender, node)?;
                    return Ok(Async::NotReady);
                }
                Ok(Async::Ready(Some(span))) => {
                    spans.push(span);
                    if spans.len() == max_batch_size {
                        Self::do_send(spans, sender, node)?;
                        // Because we've voluntarily stopped work due to a batch
                        // size limitation, notify the task to be polled again
                        // immediately.
                        task::current().notify();
                        return Ok(Async::NotReady);
                    }
                }
                Ok(Async::Ready(None)) => {
                    let _ = Self::do_send(spans, sender, node);
                    // The span receiver stream completed, so signal completion.
                    return Ok(Async::Ready(()));
                }
                Err(e) => {
                    let error: Error = e.into();
                    warn!(message="Span stream lost", %error);
                    let _ = Self::do_send(spans, sender, node);
                    return Ok(Async::Ready(()));
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
                    // If the request stream fails, all spans in this buffer
                    // will be lost.  Therefore, we keep this buffer small to
                    // minimize span loss.  Keeping this buffer small is okay
                    // because we will exert backpressure and can buffer spans
                    // elsewhere upstream.
                    // TODO: Does this make sense?
                    let (tx, rx) = mpsc::channel(1);
                    let mut svc = TraceService::new(self.client.as_service());
                    try_ready!(svc.poll_ready());
                    let req = grpc::Request::new(
                        rx.map_err(|_| grpc::Status::new(grpc::Code::Cancelled, "cancelled")),
                    );
                    trace!("Establishing new TraceService::export request");
                    let _rsp = svc.export(req);
                    State::Sending {
                        sender: tx,
                        node: Some(self.node.clone()),
                        _rsp,
                    }
                }
                State::Sending {
                    ref mut sender,
                    ref mut node,
                    ..
                } => {
                    let mut sender = sender.clone();
                    match Self::poll_send_spans(
                        &mut self.spans,
                        &mut sender,
                        node,
                        self.max_batch_size,
                    ) {
                        Ok(ready) => return Ok(ready),
                        Err(()) => State::Idle,
                    }
                }
            };
        }
    }
}
