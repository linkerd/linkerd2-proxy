#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod metrics;

use futures::stream::{Stream, StreamExt};
use http_body::Body;
use linkerd_error::Error;
use linkerd_trace_context as trace_context;
use metrics::Registry;
pub use opentelemetry as otel;
use opentelemetry::{
    trace::{SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState},
    KeyValue,
};
pub use opentelemetry_proto as proto;
use opentelemetry_proto::{
    proto::{
        collector::trace::v1::{
            trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
        },
        trace::v1::ResourceSpans,
    },
    transform::{common::ResourceAttributesWithSchema, trace::group_spans_by_resource_and_scope},
};
pub use opentelemetry_sdk as sdk;
pub use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::SpanLinks;
use tokio::{sync::mpsc, time};
use tonic::{self as grpc, body::BoxBody, client::GrpcService};
use trace_context::export::ExportSpan;
use tracing::{debug, info, trace};

pub async fn export_spans<T, S>(
    client: T,
    spans: S,
    resource: ResourceAttributesWithSchema,
    metrics: Registry,
) where
    T: GrpcService<BoxBody> + Clone,
    T::Error: Into<Error>,
    T::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
{
    debug!("Span exporter running");
    SpanExporter::new(client, spans, resource, metrics)
        .run()
        .await
}

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
struct SpanExporter<T, S> {
    client: T,
    spans: S,
    resource: ResourceAttributesWithSchema,
    metrics: Registry,
}

#[derive(Debug)]
struct SpanRxClosed;

// === impl SpanExporter ===

impl<T, S> SpanExporter<T, S>
where
    T: GrpcService<BoxBody> + Clone,
    T::Error: Into<Error>,
    T::ResponseBody: Default + Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
{
    const MAX_BATCH_SIZE: usize = 1000;
    const MAX_BATCH_IDLE: time::Duration = time::Duration::from_secs(10);

    fn new(client: T, spans: S, resource: ResourceAttributesWithSchema, metrics: Registry) -> Self {
        Self {
            client,
            spans,
            resource,
            metrics,
        }
    }

    async fn run(self) {
        let Self {
            client,
            mut spans,
            resource,
            mut metrics,
        } = self;

        // Holds the batch of pending spans. Cleared as the spans are flushed.
        // Contains no more than MAX_BATCH_SIZE spans.
        let mut accum = Vec::new();

        let mut svc = TraceServiceClient::new(client);
        loop {
            trace!("Establishing new TraceService::export request");
            metrics.start_stream();
            let (tx, mut rx) = mpsc::channel(1);

            let recv_future = async {
                while let Some(req) = rx.recv().await {
                    match svc.export(grpc::Request::new(req)).await {
                        Ok(rsp) => {
                            let Some(partial_success) = rsp.into_inner().partial_success else {
                                continue;
                            };

                            if !partial_success.error_message.is_empty() {
                                debug!(
                                    %partial_success.error_message,
                                    rejected_spans = partial_success.rejected_spans,
                                    "Response partially successful",
                                );
                            }
                        }
                        Err(error) => {
                            debug!(%error, "Response future failed; restarting");
                        }
                    }
                }
            };

            // Drive both the response future and the export stream
            // simultaneously.
            tokio::select! {
                _ = recv_future => {}
                res = Self::export(&tx, &mut spans, &resource, &mut accum) => match res {
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
        resource: &ResourceAttributesWithSchema,
        accum: &mut Vec<ResourceSpans>,
    ) -> Result<(), SpanRxClosed> {
        loop {
            // Collect spans into a batch.
            let collect = Self::collect_batch(spans, resource, accum).await;

            // If we collected spans, flush them.
            if !accum.is_empty() {
                // Once a batch has been accumulated, ensure that the
                // request stream is ready to accept the batch.
                match tx.reserve().await {
                    Ok(tx) => {
                        let msg = ExportTraceServiceRequest {
                            resource_spans: std::mem::take(accum),
                        };
                        trace!(spans = msg.resource_spans.len(), "Sending batch");
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
    /// Returns an error when the span stream has completed. An error may be
    /// returned after accumulating spans.
    async fn collect_batch(
        span_stream: &mut S,
        resource: &ResourceAttributesWithSchema,
        accum: &mut Vec<ResourceSpans>,
    ) -> Result<(), SpanRxClosed> {
        let mut input_accum: Vec<SpanData> = vec![];

        let res = loop {
            if input_accum.len() == Self::MAX_BATCH_SIZE {
                trace!(capacity = Self::MAX_BATCH_SIZE, "Batch capacity reached");
                break Ok(());
            }

            tokio::select! {
                biased;

                res = span_stream.next() => match res {
                    Some(span) => {
                        trace!(?span, "Adding to batch");
                        let span = match convert_span(span) {
                            Ok(span) => span,
                            Err(error) => {
                                info!(%error, "Span dropped");
                                continue;
                            }
                        };

                        input_accum.push(span);
                    }
                    None => break Err(SpanRxClosed),
                },

                // Don't hold spans indefinitely. Return if we hit an idle
                // timeout and spans have been collected.
                _ = time::sleep(Self::MAX_BATCH_IDLE) => {
                    if !input_accum.is_empty() {
                        trace!(spans = input_accum.len(), "Flushing spans due to inactivitiy");
                        break Ok(());
                    }
                }
            }
        };

        *accum = group_spans_by_resource_and_scope(input_accum, resource);

        res
    }
}

fn convert_span(span: ExportSpan) -> Result<SpanData, Error> {
    let ExportSpan { span, kind, labels } = span;

    let mut attributes = Vec::<KeyValue>::new();
    for (k, v) in labels.iter() {
        attributes.push(KeyValue::new(k.clone(), v.clone()));
    }
    let is_remote = kind != trace_context::export::SpanKind::Client;
    Ok(SpanData {
        parent_span_id: SpanId::from_bytes(span.parent_id.into_bytes()?),
        span_kind: match kind {
            trace_context::export::SpanKind::Server => SpanKind::Server,
            trace_context::export::SpanKind::Client => SpanKind::Client,
        },
        name: span.span_name.into(),
        start_time: span.start,
        end_time: span.end,
        attributes,
        dropped_attributes_count: 0,
        links: SpanLinks::default(),
        status: Status::Unset, // TODO: this is gRPC status; we must read response trailers to populate this
        span_context: SpanContext::new(
            TraceId::from_bytes(span.trace_id.into_bytes()?),
            SpanId::from_bytes(span.span_id.into_bytes()?),
            TraceFlags::default(),
            is_remote,
            TraceState::NONE,
        ),
        events: Default::default(),
        instrumentation_scope: Default::default(),
    })
}
