#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod metrics;

use self::metrics::Registry;
use futures::stream::{Stream, StreamExt};
use http_body::Body;
use linkerd_error::Error;
use linkerd_trace_context::{self as trace_context, export::ExportSpan};
pub use opentelemetry as otel;
use opentelemetry::{
    trace::{SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState},
    KeyValue,
};
pub use opentelemetry_proto as proto;
use opentelemetry_proto::{
    tonic::{
        collector::trace::v1::{
            trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
        },
        trace::v1::ResourceSpans,
    },
    transform::{
        common::tonic::ResourceAttributesWithSchema,
        trace::tonic::group_spans_by_resource_and_scope,
    },
};
use opentelemetry_sdk::trace::{SpanData, SpanLinks};
pub use opentelemetry_sdk::{self as sdk};
use tokio::{
    sync::mpsc,
    time::{self, Instant, MissedTickBehavior},
};
use tonic::{self as grpc, body::Body as TonicBody, client::GrpcService};
use tracing::{debug, info, trace};

pub async fn export_spans<T, S>(
    client: T,
    spans: S,
    resource: ResourceAttributesWithSchema,
    metrics: Registry,
) where
    T: GrpcService<TonicBody> + Clone,
    T::Error: Into<Error>,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
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
    T: GrpcService<TonicBody> + Clone,
    T::Error: Into<Error>,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
{
    const MAX_BATCH_SIZE: usize = 1000;
    const BATCH_INTERVAL: time::Duration = time::Duration::from_secs(10);

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

        let mut interval =
            time::interval_at(Instant::now() + Self::BATCH_INTERVAL, Self::BATCH_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

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

                // Don't hold spans indefinitely. Return if we hit an interval tick and spans have
                // been collected.
                _ = interval.tick() => {
                    if !input_accum.is_empty() {
                        trace!(spans = input_accum.len(), "Flushing spans due to interval tick");
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
    for (k, v) in span.labels.into_iter() {
        attributes.push(KeyValue::new(k, v));
    }
    let is_remote = kind != trace_context::export::SpanKind::Client;
    Ok(SpanData {
        parent_span_id: SpanId::from_bytes(span.parent_id.into_bytes()?),
        parent_span_is_remote: true, // we do not originate any spans locally
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

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_trace_context::{export::SpanKind, Id, Span};
    use opentelemetry_proto::tonic::{common::v1::InstrumentationScope, resource::v1::Resource};
    use std::{collections::HashMap, sync::Arc, time::SystemTime};
    use tokio::sync::mpsc;
    use tonic::codegen::{tokio_stream::wrappers::ReceiverStream, tokio_stream::StreamExt, Bytes};
    use tonic_prost::ProstDecoder;

    #[tokio::test(flavor = "current_thread")]
    async fn send_span() {
        let trace_id = Id::from(Bytes::from(
            hex::decode("0123456789abcedffedcba9876543210").expect("decode"),
        ));
        let span_id = Id::from(Bytes::from(
            hex::decode("fedcba9876543210").expect("decode"),
        ));
        let parent_id = Id::from(Bytes::from(
            hex::decode("0123456789abcedf").expect("decode"),
        ));
        let span_name = "test".to_string();

        let start = SystemTime::now();
        let end = SystemTime::now();

        let span = ExportSpan {
            span: Span {
                trace_id: trace_id.clone(),
                span_id: span_id.clone(),
                parent_id: parent_id.clone(),
                span_name: span_name.clone(),
                start,
                end,
                labels: HashMap::new(),
            },
            kind: SpanKind::Server,
            labels: Arc::new(Default::default()),
        };

        let mut req = send_mock_request(span).await;

        assert_eq!(req.resource_spans.len(), 1);
        let mut resource_span = req.resource_spans.remove(0);
        assert_eq!(
            resource_span.resource,
            Some(Resource {
                attributes: vec![],
                dropped_attributes_count: 0,
                entity_refs: vec![],
            })
        );
        assert_eq!(resource_span.schema_url, "");
        assert_eq!(resource_span.scope_spans.len(), 1);

        let mut scope_span = resource_span.scope_spans.remove(0);
        assert_eq!(scope_span.schema_url, "");
        assert_eq!(
            scope_span.scope,
            Some(InstrumentationScope {
                name: "".to_string(),
                version: "".to_string(),
                attributes: vec![],
                dropped_attributes_count: 0,
            })
        );
        assert_eq!(scope_span.spans.len(), 1);

        let span = scope_span.spans.remove(0);
        assert_eq!(
            span.span_id,
            span_id.into_bytes::<8>().expect("into_bytes").to_vec()
        );
        assert_eq!(
            span.parent_span_id,
            parent_id.into_bytes::<8>().expect("into_bytes").to_vec()
        );
        assert_eq!(
            span.trace_id,
            trace_id.into_bytes::<16>().expect("into_bytes").to_vec()
        );
        assert_eq!(span.name, span_name);
        assert_eq!(
            span.start_time_unix_nano,
            start
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("duration")
                .as_nanos() as u64
        );
        assert_eq!(
            span.end_time_unix_nano,
            end.duration_since(SystemTime::UNIX_EPOCH)
                .expect("duration")
                .as_nanos() as u64
        );
        assert_eq!(span.flags, 0b11_0000_0000);
    }

    async fn send_mock_request(span: ExportSpan) -> ExportTraceServiceRequest {
        let _trace = linkerd_tracing::test::trace_init();
        let (span_tx, span_rx) = mpsc::channel(1);

        let (inner, mut handle) = tower_test::mock::pair::<
            http::Request<tonic::body::Body>,
            http::Response<tonic::body::Body>,
        >();
        handle.allow(1);

        span_tx.try_send(span).expect("Must have space");

        let (metrics, _) = metrics::new();
        tokio::spawn(export_spans(
            inner,
            ReceiverStream::new(span_rx),
            ResourceAttributesWithSchema::default(),
            metrics,
        ));

        let (req, _tx) = handle.next_request().await.expect("next request");
        let req = tonic::Request::from_http(req);
        let mut req = tonic::codec::Streaming::<ExportTraceServiceRequest>::new_request(
            ProstDecoder::default(),
            req.into_inner(),
            None,
            None,
        );
        let req = req.next().await.expect("next").expect("must succeed");

        req
    }
}
