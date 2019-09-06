use futures::{try_ready, Async, Future, Poll, Stream};
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    client::TraceService, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opencensus_proto::trace::v1::Span;
use tokio::sync::mpsc;
use tower_grpc::{
    self as grpc, client::streaming::ResponseFuture, generic::client::GrpcService, BoxBody,
};
use tracing::{debug, trace};

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
}

enum State<T: GrpcService<BoxBody>> {
    Idle,
    Sending {
        sender: mpsc::Sender<ExportTraceServiceRequest>,
        // Node data should only be sent on the first message of a streaming
        // request.
        sent_node: bool,
        // We hold the response future, but never poll it.
        rsp: Option<ResponseFuture<ExportTraceServiceResponse, T::Future>>,
    },
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
    S::Error: Into<Error>,
{
    pub fn new(client: T, node: Node, spans: S) -> Self {
        Self {
            client,
            node,
            spans,
            state: State::Idle,
        }
    }

    /// Attempt to read a single span from the spans stream and write it to
    /// the export streaming request.  Returns NotReady there isn't room in
    /// the streaming request or if there isn't a span available in the spans
    /// stream.  Returns Ready if a span was transfered successfully.  Returns
    /// Err if any operation fails, which indicates a new streaming request
    /// must be established.
    fn poll_send_span(
        &mut self,
        sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
        sent_node: bool,
    ) -> Poll<(), Error> {
        try_ready!(sender.poll_ready());
        let span = try_ready!(self.spans.poll().map_err(Into::into))
            .expect("span stream should never terminate");
        let node = if sent_node {
            None
        } else {
            Some(self.node.clone())
        };
        let msg = ExportTraceServiceRequest {
            node,
            spans: vec![span],
            resource: None,
        };
        trace!("transmitting span: {:?}", msg);
        sender
            .try_send(msg)
            .map(|()| Async::Ready(()))
            .map_err(|e| {
                debug!("failed to transmit span: {}", e);
                e.into()
            })
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
                    trace!("establishing new TraceService::export request");
                    let rsp = svc.export(req);
                    State::Sending {
                        sender: tx,
                        rsp: Some(rsp),
                        sent_node: false,
                    }
                }
                State::Sending {
                    ref sender,
                    ref mut rsp,
                    sent_node,
                } => {
                    let mut sender = sender.clone();
                    let rsp = rsp.take();
                    match self.poll_send_span(&mut sender, sent_node) {
                        Ok(Async::Ready(())) => State::Sending {
                            sender: sender,
                            rsp,
                            sent_node: true,
                        },
                        Ok(Async::NotReady) => {
                            return Ok(Async::NotReady);
                        }
                        Err(_) => State::Idle,
                    }
                }
            };
        }
    }
}
