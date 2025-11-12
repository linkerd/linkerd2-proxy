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
    tonic::collector::trace::v1::{
        trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
    },
    transform::trace::tonic::group_spans_by_resource_and_scope,
};
pub use opentelemetry_sdk as sdk;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, SpanData, SpanLinks, SpanProcessor,
};
use opentelemetry_sdk::Resource;
pub use opentelemetry_semantic_conventions as semconv;
use std::fmt::{Debug, Formatter};
use std::time::Duration;
use tonic::{body::Body as TonicBody, client::GrpcService};
use tracing::{debug, info};

pub async fn export_spans<T, S>(client: T, spans: S, resource: Resource, metrics: Registry)
where
    T: GrpcService<TonicBody> + Clone + Send + Sync + 'static,
    T::Error: Into<Error>,
    T::Future: Send,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
    S: Stream<Item = ExportSpan> + Unpin,
{
    debug!("Span exporter running");

    let processor = BatchSpanProcessor::builder(SpanExporter {
        client: TraceServiceClient::new(client),
        resource,
        metrics,
    })
    .with_batch_config(
        BatchConfigBuilder::default()
            .with_max_queue_size(1000)
            .with_scheduled_delay(Duration::from_secs(5))
            .build(),
    )
    .build();

    SpanExportTask::new(spans, processor).run().await;
}

/// SpanExporter sends a Stream of spans to the given TraceService gRPC service.
struct SpanExporter<T> {
    client: TraceServiceClient<T>,
    resource: Resource,
    metrics: Registry,
}

impl<T> Debug for SpanExporter<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanExporter").finish_non_exhaustive()
    }
}

impl<T> opentelemetry_sdk::trace::SpanExporter for SpanExporter<T>
where
    T: GrpcService<TonicBody> + Clone + Send + Sync,
    <T as GrpcService<TonicBody>>::Future: Send,
    T::Error: Into<Error>,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
{
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let mut metrics = self.metrics.clone();
        metrics.start_stream();
        let span_count = batch.len();
        let mut client = self.client.clone();

        debug!("Exporting {span_count} spans");

        let resource_spans = group_spans_by_resource_and_scope(batch, &(&self.resource).into());

        match client
            .export(ExportTraceServiceRequest { resource_spans })
            .await
        {
            Ok(resp) => {
                metrics.send(span_count as u64);
                if let Some(partial) = resp.into_inner().partial_success {
                    if !partial.error_message.is_empty() {
                        debug!(
                            %partial.error_message,
                            rejected_spans = partial.rejected_spans,
                            "Response partially successful",
                        );
                        return Err(OTelSdkError::InternalFailure(partial.error_message));
                    }
                }
            }
            Err(e) => {
                return Err(OTelSdkError::InternalFailure(e.to_string()));
            }
        }

        Ok(())
    }
}

struct SpanExportTask<S> {
    spans: S,
    processor: BatchSpanProcessor,
}

impl<S> SpanExportTask<S>
where
    S: Stream<Item = ExportSpan> + Unpin,
{
    fn new(spans: S, processor: BatchSpanProcessor) -> Self {
        Self { spans, processor }
    }

    async fn run(mut self) {
        while let Some(span) = self.spans.next().await {
            let s = match convert_span(span) {
                Ok(s) => s,
                Err(error) => {
                    info!(%error, "Span dropped");
                    continue;
                }
            };

            self.processor.on_end(s);
        }
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
    use opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue;
    use opentelemetry_proto::tonic::common::v1::AnyValue;
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
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
        let resource_span_resource = resource_span.resource.expect("must have resource");
        assert_eq!(resource_span_resource.dropped_attributes_count, 0);
        assert_eq!(resource_span_resource.entity_refs, vec![]);
        resource_span_resource
            .attributes
            .iter()
            .any(|attr| attr.key == "telemetry.sdk.name");
        resource_span_resource
            .attributes
            .iter()
            .any(|attr| attr.key == "telemetry.sdk.version");
        resource_span_resource
            .attributes
            .iter()
            .any(|attr| attr.key == "telemetry.sdk.language");
        resource_span_resource
            .attributes
            .iter()
            .any(|attr| attr.key == "service.name");
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
            opentelemetry_sdk::Resource::builder().build(),
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
