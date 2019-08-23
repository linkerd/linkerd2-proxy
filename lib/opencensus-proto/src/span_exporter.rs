use super::gen::agent::common::v1::Node;
use super::gen::agent::trace::v1::{
    client::TraceService, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use super::gen::trace::v1::Span;
use crate::tokio::sync::mpsc;
use futures::{try_ready, Async, Future, Poll, Stream};
use tower_grpc::{
    self as grpc, client::streaming::ResponseFuture, generic::client::GrpcService, BoxBody,
};
use tracing::trace;

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
pub struct SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
{
    client: T,
    node: Node,
    state: Option<State<T>>,
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
        rsp: ResponseFuture<ExportTraceServiceResponse, T::Future>,
    },
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
{
    pub fn new(client: T, node: Node, spans: S) -> Self {
        Self {
            client,
            node,
            spans,
            state: Some(State::Idle),
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
        send_node: bool,
    ) -> Poll<(), ()> {
        try_ready!(sender.poll_ready().map_err(|_| ()));
        let span = try_ready!(self.spans.poll().map_err(|_| ()))
            .expect("span stream should never terminate");
        let node = if send_node {
            Some(self.node.clone())
        } else {
            None
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
            .map_err(|_| ())
    }
}

impl<T, S> Future for SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    S: Stream<Item = Span>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let state = match self.state.take().expect("corrupt state") {
                State::Idle => {
                    // If the request stream fails, all spans in this buffer
                    // will be lost.  Therefore, we keep this buffer small to
                    // minimize span loss.  Keeping this buffer small is okay
                    // because we will exert backpressure and can buffer spans
                    // elsewhere upstream.
                    // TODO: Does this make sense?
                    let (tx, rx) = mpsc::channel(1);
                    let mut svc = TraceService::new(self.client.as_service());
                    let req = grpc::Request::new(
                        rx.map_err(|_| grpc::Status::new(grpc::Code::Cancelled, "cancelled")),
                    );
                    trace!("establishing new TraceService::export request");
                    let rsp = svc.export(req);
                    State::Sending {
                        sender: tx,
                        rsp,
                        sent_node: false,
                    }
                }
                State::Sending {
                    mut sender,
                    rsp,
                    sent_node,
                } => match self.poll_send_span(&mut sender, sent_node) {
                    Ok(Async::Ready(())) => State::Sending {
                        sender,
                        rsp,
                        sent_node: true,
                    },
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(_) => State::Idle,
                },
            };
            self.state = Some(state);
        }
    }
}
