#![deny(warnings, rust_2018_idioms)]
use futures::stream::{Stream, StreamExt};
use http_body::Body as HttpBody;
use linkerd_channel::into_stream::IntoStream;
use linkerd_error::Error;
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use opencensus_proto::trace::v1::Span;
use std::convert::TryInto;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tracing::trace;
pub mod metrics;

pub async fn export_spans<T, S>(client: T, node: Node, spans: S, metrics: Registry)
where
    T: GrpcService<BoxBody> + Clone,
    T::Error: Into<Error>,
    <T::ResponseBody as HttpBody>::Error: Into<Error> + Send + Sync,
    T::ResponseBody: 'static,
    S: Stream<Item = Span> + Unpin,
{
    SpanExporter::new(client, node, spans, metrics).run().await
}

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
struct SpanExporter<T, S> {
    client: T,
    node: Node,
    spans: S,
    metrics: Registry,
}

// ===== impl SpanExporter =====

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody> + Clone,
    T::Error: Into<Error>,
    <T::ResponseBody as HttpBody>::Error: Into<Error> + Send + Sync,
    T::ResponseBody: 'static,
    S: Stream<Item = Span> + Unpin,
{
    fn new(client: T, node: Node, spans: S, metrics: Registry) -> Self {
        Self {
            client,
            node,
            spans,
            metrics,
        }
    }

    async fn run(mut self) {
        'reconnect: loop {
            let (request_tx, request_rx) = mpsc::channel(1);
            let mut svc = TraceServiceClient::new(self.client.clone());
            let req = grpc::Request::new(request_rx.into_stream());
            trace!("Establishing new TraceService::export request");
            self.metrics.start_stream();
            let mut rsp = Box::pin(svc.export(req));
            let mut drive_rsp = true;

            loop {
                tokio::select! {
                    res = &mut rsp, if drive_rsp => match res {
                        Ok(_) => {
                            drive_rsp = false;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "response future failed, sending a new request");
                            continue 'reconnect;
                        }
                    },
                    res = self.send_batch(&request_tx) => match res {
                        Ok(false) => return, // The span strean has ended --- the proxy is shutting down.
                        Ok(true) => {} // Continue the inner loop and send a new batch of spans.
                        Err(()) => continue 'reconnect,
                    }
                }
            }
        }
    }

    async fn send_batch(&mut self, tx: &mpsc::Sender<ExportTraceServiceRequest>) -> Result<(), ()> {
        const MAX_BATCH_SIZE: usize = 100;
        // If the sender is dead, return an error so we can reconnect.
        let send = tx.reserve().await.map_err(|_| ())?;

        let mut spans = Vec::new();
        let mut can_send_more = false;
        while let Some(span) = self.spans.next().await {
            spans.push(span);
            if spans.len() == MAX_BATCH_SIZE {
                can_send_more = true;
                break;
            }
        }

        if spans.is_empty() {
            return Ok(false);
        }

        if let Ok(num_spans) = spans.len().try_into() {
            self.metrics.send(num_spans);
        }
        let req = ExportTraceServiceRequest {
            spans,
            node: Some(self.node.clone()),
            resource: None,
        };
        trace!(message = "Transmitting", ?req);
        sender.send(req);
        Ok(can_send_more)
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
