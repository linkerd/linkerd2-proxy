#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod metrics;
pub mod propagation;

use self::metrics::Registry;
use crate::propagation::OrderedPropagator;
use http_body::Body;
use linkerd_error::Error;
pub use opentelemetry as otel;
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
    BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider, SpanData,
};
use opentelemetry_sdk::Resource;
pub use opentelemetry_semantic_conventions as semconv;
use std::fmt::{Debug, Formatter};
use std::time::Duration;
use tonic::{body::Body as TonicBody, client::GrpcService};
use tracing::debug;

pub fn install_opentelemetry_providers<T>(client: T, resource: Resource, metrics: Registry)
where
    T: GrpcService<TonicBody> + Clone + Send + Sync + 'static,
    T::Error: Into<Error>,
    T::Future: Send,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
{
    debug!("Span exporter running");

    let provider = SdkTracerProvider::builder()
        .with_span_processor(
            BatchSpanProcessor::builder(SpanExporter {
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
            .build(),
        )
        .build();

    opentelemetry::global::set_tracer_provider(provider);
    opentelemetry::global::set_text_map_propagator(OrderedPropagator::new());
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

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, TraceContextExt, TraceState, Tracer};
    use opentelemetry::{Context, SpanId, TraceFlags, TraceId};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope};
    use std::collections::HashMap;
    use tonic::codegen::tokio_stream::StreamExt;
    use tonic_prost::ProstDecoder;

    #[tokio::test(flavor = "current_thread")]
    async fn send_span() {
        let _trace = linkerd_tracing::test::trace_init();

        let trace_id = TraceId::from_hex("0123456789abcedffedcba9876543210").expect("trace id");
        let parent_id = SpanId::from_hex("fedcba9876543210").expect("parent id");
        let span_id = SpanId::from_hex("0123456789abcedf").expect("span id");
        let span_name = "test-span".to_string();

        let mut req = send_mock_request(trace_id, parent_id, span_id, span_name.clone()).await;

        assert_eq!(req.resource_spans.len(), 1);
        let mut resource_span = req.resource_spans.remove(0);
        let resource_span_resource = resource_span.resource.expect("must have resource");
        assert_eq!(resource_span_resource.dropped_attributes_count, 0);
        assert_eq!(resource_span_resource.entity_refs, vec![]);
        let expected: HashMap<&str, Option<&str>> = HashMap::from_iter([
            ("telemetry.sdk.name", Some("opentelemetry")),
            ("telemetry.sdk.version", None),
            ("telemetry.sdk.language", Some("rust")),
            ("service.name", Some("unknown_service")),
        ]);

        assert_eq!(expected.len(), resource_span_resource.attributes.len());
        for (expected_key, expected_value) in expected {
            let Some(actual) = resource_span_resource
                .attributes
                .iter()
                .find(|kv| kv.key == expected_key)
            else {
                panic!("{expected_key} not found in resource attributes");
            };

            let Some(AnyValue {
                value: Some(Value::StringValue(actual_value)),
            }) = &actual.value
            else {
                panic!("Unexpected value type: {:?}", actual.value);
            };

            if let Some(expected_value) = expected_value {
                assert_eq!(actual_value, expected_value);
            }
        }

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
                name: "test".to_string(),
                version: "".to_string(),
                attributes: vec![],
                dropped_attributes_count: 0,
            })
        );

        assert_eq!(scope_span.spans.len(), 1);
        let span = scope_span.spans.remove(0);

        assert_eq!(span.span_id, span_id.to_bytes().to_vec(),);
        assert_eq!(span.parent_span_id, parent_id.to_bytes().to_vec(),);
        assert_eq!(span.trace_id, trace_id.to_bytes().to_vec(),);
        assert_eq!(span.name, span_name);
        assert_eq!(span.flags, 0b11_0000_0001);
    }

    async fn send_mock_request(
        trace_id: TraceId,
        parent_id: SpanId,
        span_id: SpanId,
        span_name: String,
    ) -> ExportTraceServiceRequest {
        let (inner, mut handle) = tower_test::mock::pair::<
            http::Request<tonic::body::Body>,
            http::Response<tonic::body::Body>,
        >();
        handle.allow(1);

        let (metrics, _) = metrics::new();
        install_opentelemetry_providers(inner, Resource::builder().build(), metrics);

        let parent_cx = Context::new().with_remote_span_context(SpanContext::new(
            trace_id,
            parent_id,
            TraceFlags::SAMPLED,
            true,
            TraceState::NONE,
        ));

        let tracer = opentelemetry::global::tracer("test");
        let span = tracer
            .span_builder(span_name)
            .with_span_id(span_id)
            .start_with_context(&tracer, &parent_cx);
        drop(span);

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
