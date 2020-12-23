#![deny(warnings, rust_2018_idioms)]
use http_body::Body as HttpBody;
use linkerd2_error::Error;
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use opencensus_proto::trace::v1::Span;
use std::task::{Context, Poll};
use tokio::{
    stream::{Stream, StreamExt},
    sync::mpsc,
};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tracing::{debug, trace};
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
    const MAX_BATCH_SIZE: usize = 100;

    fn new(client: T, node: Node, spans: S, metrics: Registry) -> Self {
        Self {
            client,
            node,
            spans,
            metrics,
        }
    }

    async fn run(mut self) {
        let mut spans = Vec::with_capacity(Self::MAX_BATCH_SIZE);
        let mut svc = TraceServiceClient::new(self.client.clone());
        loop {
            let (request_tx, request_rx) = mpsc::channel(1);
            let req = grpc::Request::new(request_rx);
            trace!("Establishing new TraceService::export request");
            match svc.export(req).await {
                Ok(_) => {
                    self.metrics.start_stream();

                    // The node is included on only the first message of every
                    // stream.
                    let mut node = Some(self.node.clone());

                    // Send batches as long as the gRPC channel is open.
                    while let Ok(tx) = request_tx.reserve().await {
                        // Collect spans and then send them.
                        let collect = self.collect_batch(&mut spans).await;
                        if !spans.is_empty() {
                            let msg = ExportTraceServiceRequest {
                                spans: spans.drain(..).collect(),
                                node: node.take(),
                                ..Default::default()
                            };
                            trace!(?msg, "Sending");
                            tx.send(msg);
                        }
                        if collect.is_err() {
                            debug!("Span channel lost");
                            return;
                        }
                    }
                }
                Err(error) => {
                    debug!(%error, "Response future failed; restarting");
                }
            }
        }
    }

    async fn collect_batch(&mut self, spans: &mut Vec<Span>) -> Result<(), ()> {
        loop {
            let s = self.spans.next().await.ok_or(())?;
            spans.push(s);
            if spans.len() == Self::MAX_BATCH_SIZE {
                return Ok(());
            }
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
