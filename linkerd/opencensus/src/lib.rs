#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod metrics;

use futures::stream::{Stream, StreamExt};
use http_body::Body;
use linkerd_error::Error;
use linkerd_trace_context::export::{ExportSpan, SpanKind};
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::{
    agent::{
        common::v1::Node,
        trace::v1::{trace_service_client::TraceServiceClient, ExportTraceServiceRequest},
    },
    trace::v1::{Span, TruncatableString},
};
use std::collections::HashMap;
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use tracing::{debug, info, trace};

pub async fn export_spans<T, S>(client: T, node: Node, spans: S, metrics: Registry)
where
    T: GrpcService<BoxBody> + Clone,
    T::Error: Into<Error>,
    T::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
{
    debug!("Span exporter running");
    SpanExporter::new(client, node, spans, metrics).run().await
}

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
struct SpanExporter<T, S> {
    client: T,
    node: Node,
    spans: S,
    metrics: Registry,
}

#[derive(Debug)]
struct SpanRxClosed;

// === impl SpanExporter ===

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody>,
    T::Error: Into<Error>,
    T::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
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
            metrics.start_stream();
            let (tx, rx) = mpsc::channel(1);

            // The node is only transmitted on the first message of each stream.
            let mut node = Some(node.clone());

            // Drive both the response future and the export stream
            // simultaneously.
            tokio::select! {
                res = svc.export(grpc::Request::new(ReceiverStream::new(rx))) => match res {
                    Ok(_rsp) => {
                        // The response future completed. Continue exporting spans until the
                        // stream stops accepting them.
                        if let Err(SpanRxClosed) = Self::export(&tx, &mut spans, &mut accum, &mut node).await {
                            // No more spans.
                            return;
                        }
                    }
                    Err(error) => {
                        debug!(%error, "Response future failed; restarting");
                    }
                },
                res = Self::export(&tx, &mut spans, &mut accum, &mut node) => match res {
                    // The export stream closed; reconnect.
                    Ok(()) => {},
                    // No more spans.
                    Err(SpanRxClosed) => return,
                },
            }
        }
    }

    /// Accumulate spans and send them on the export stream.
    ///
    /// Returns an error when the proxy has closed the span stream.
    async fn export(
        tx: &mpsc::Sender<ExportTraceServiceRequest>,
        spans: &mut S,
        accum: &mut Vec<Span>,
        node: &mut Option<Node>,
    ) -> Result<(), SpanRxClosed> {
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
                            spans: std::mem::take(accum),
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

            // If the span source was closed, end the task.
            if let Err(closed) = collect {
                debug!("Span channel lost");
                return Err(closed);
            }
        }
    }

    /// Collects spans from the proxy into `accum`.
    ///
    /// Returns an error when the span sream has completed. An error may be
    /// returned after accumulating spans.
    async fn collect_batch(spans: &mut S, accum: &mut Vec<Span>) -> Result<(), SpanRxClosed> {
        loop {
            if accum.len() == Self::MAX_BATCH_SIZE {
                trace!(capacity = Self::MAX_BATCH_SIZE, "Batch capacity reached");
                return Ok(());
            }

            tokio::select! {
                biased;

                res = spans.next() => match res {
                    Some(span) => {
                        trace!(?span, "Adding to batch");
                        let span = match convert_span(span) {
                            Ok(span) => span,
                            Err(error) => {
                                info!(%error, "Span dropped");
                                continue;
                            }
                        };
                        accum.push(span);
                    }
                    None => return Err(SpanRxClosed),
                },

                // Don't hold spans indefinitely. Return if we hit an idle
                // timeout and spans have been collected.
                _ = time::sleep(Self::MAX_BATCH_IDLE) => {
                    if !accum.is_empty() {
                        trace!(spans = accum.len(), "Flushing spans due to inactivitiy");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn convert_span(span: ExportSpan) -> Result<Span, Error> {
    use proto::trace::v1 as oc;

    let ExportSpan {
        mut span,
        kind,
        labels,
    } = span;

    let mut attributes = HashMap::<String, oc::AttributeValue>::new();
    for (k, v) in labels.iter() {
        attributes.insert(
            k.clone(),
            oc::AttributeValue {
                value: Some(oc::attribute_value::Value::StringValue(truncatable(
                    v.clone(),
                ))),
            },
        );
    }
    for (k, v) in span.labels.drain() {
        attributes.insert(
            k.to_string(),
            oc::AttributeValue {
                value: Some(oc::attribute_value::Value::StringValue(truncatable(v))),
            },
        );
    }
    Ok(Span {
        trace_id: span.trace_id.into_bytes::<16>()?.to_vec(),
        span_id: span.span_id.into_bytes::<8>()?.to_vec(),
        tracestate: None,
        parent_span_id: span.parent_id.into_bytes::<8>()?.to_vec(),
        name: Some(truncatable(span.span_name)),
        kind: kind as i32,
        start_time: Some(span.start.into()),
        end_time: Some(span.end.into()),
        attributes: Some(oc::span::Attributes {
            attribute_map: attributes,
            dropped_attributes_count: 0,
        }),
        stack_trace: None,
        time_events: None,
        links: None,
        status: None, // TODO: this is gRPC status; we must read response trailers to populate this
        resource: None,
        same_process_as_parent_span: Some(kind == SpanKind::Client),
        child_span_count: None,
    })
}

fn truncatable(value: String) -> TruncatableString {
    TruncatableString {
        value,
        truncated_byte_count: 0,
    }
}
