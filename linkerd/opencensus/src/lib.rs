#![deny(warnings, rust_2018_idioms)]

pub mod metrics;

use futures::FutureExt;
use http_body::Body as HttpBody;
use linkerd2_error::Error;
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use opencensus_proto::trace::v1::Span;
use tokio::{
    stream::{Stream, StreamExt},
    sync::mpsc,
    time,
};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tracing::{debug, trace};

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
    T: GrpcService<BoxBody>,
    T::Error: Into<Error>,
    <T::ResponseBody as HttpBody>::Error: Into<Error> + Send + Sync,
    T::ResponseBody: 'static,
    S: Stream<Item = Span> + Unpin,
{
    const MAX_BATCH_SIZE: usize = 1000;
    const MAX_BATCH_IDLE: time::Duration = time::Duration::from_secs(10);

    fn new(client: T, node: Node, spans: S, metrics: Registry) -> Self {
        Self {
            client,
            node,
            spans,
            metrics,
        }
    }

    async fn run(self) {
        let Self {
            client,
            node,
            mut spans,
            mut metrics,
        } = self;

        // Holds the batch of pending spans. Cleared as the spans are flushed.
        // Contains no more than MAX_BATCH_SIZE spans.
        let mut accum = Vec::new();

        let mut svc = TraceServiceClient::new(client);
        loop {
            trace!("Establishing new TraceService::export request");
            let (tx, rx) = mpsc::channel(1);
            match svc.export(grpc::Request::new(rx)).await {
                Ok(_) => {
                    metrics.start_stream();
                    if Self::stream(tx, &mut spans, &mut accum, node.clone())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    debug!(%error, "Response future failed; restarting");
                }
            }
        }
    }

    async fn stream(
        tx: mpsc::Sender<ExportTraceServiceRequest>,
        spans: &mut S,
        accum: &mut Vec<Span>,
        node: Node,
    ) -> Result<(), ()> {
        // The node is only transmitted on the first message of each stream.
        let mut node = Some(node);

        loop {
            // Collect spans into a batch.
            let collect = Self::collect_batch(spans, accum).await;

            // If we collected spans, flush them.
            if !accum.is_empty() {
                // Once a batch has been accumulated, ensure that the
                // request stream is ready to accept the batch.
                match tx.reserve().await {
                    Ok(tx) => {
                        let msg = ExportTraceServiceRequest {
                            spans: accum.drain(..).collect(),
                            node: node.take(),
                            ..Default::default()
                        };
                        trace!(
                            node = msg.node.is_some(),
                            spans = msg.spans.len(),
                            "Sending batch"
                        );
                        tx.send(msg);
                    }
                    Err(error) => {
                        // If the channel isn't open, start a new stream
                        // and retry sending the batch.
                        debug!(%error, "Request stream lost; restarting");
                        return Ok(());
                    }
                }
            }

            // The span source was closed, so end the task.
            if collect.is_err() {
                debug!("Span channel lost");
                return Err(());
            }
        }
    }

    async fn collect_batch(spans: &mut S, accum: &mut Vec<Span>) -> Result<(), ()> {
        loop {
            if accum.len() == Self::MAX_BATCH_SIZE {
                trace!(capacity = Self::MAX_BATCH_SIZE, "Batch capacity reached");
                return Ok(());
            }

            futures::select_biased! {
                res = spans.next().fuse() => match res {
                    Some(span) => {
                        trace!(?span, "Adding to batch");
                        accum.push(span);
                    }
                    None => return Err(()),
                },
                // Don't hold spans indefinitely. Return if we hit an idle
                // timeout and spans have been collected.
                _ = time::sleep(Self::MAX_BATCH_IDLE).fuse() => {
                    if !accum.is_empty() {
                        trace!(spans = accum.len(), "Flushing spans due to inactivitiy");
                        return Ok(());
                    }
                }
            }
        }
    }
}
