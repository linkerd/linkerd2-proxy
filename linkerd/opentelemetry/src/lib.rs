#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod metrics;

use self::metrics::Registry;
use http_body::Body;
use linkerd_error::Error;
pub use opentelemetry as otel;
use opentelemetry::propagation::text_map_propagator::FieldIter;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::Context;
pub use opentelemetry_proto as proto;
use opentelemetry_proto::{
    tonic::collector::trace::v1::{
        trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
    },
    transform::trace::tonic::group_spans_by_resource_and_scope,
};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SpanData};
use opentelemetry_sdk::Resource;
pub use opentelemetry_sdk::{self as sdk};
pub use opentelemetry_semantic_conventions as semconv;
use std::fmt::{Debug, Formatter};
use std::time::Duration;
use tonic::{body::Body as TonicBody, client::GrpcService};
use tracing::debug;

pub fn setup_global_trace_exporter<T>(client: T, resource: Resource, metrics: Registry)
where
    T: GrpcService<TonicBody> + Clone + Send + Sync + 'static,
    <T as GrpcService<TonicBody>>::Future: Send,
    T::Error: Into<Error>,
    T::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<Error> + Send,
{
    opentelemetry::global::set_text_map_propagator(OrderedPropagator::new());

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
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

    opentelemetry::global::set_tracer_provider(tracer_provider);

    debug!("Span exporter running");
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

#[derive(Copy, Clone, Eq, PartialEq)]
enum PropagationFormat {
    W3C,
    B3,
}

#[derive(Debug)]
struct OrderedPropagator {
    w3c: TraceContextPropagator,
    b3: opentelemetry_zipkin::Propagator,
    baggage: BaggagePropagator,
    fields: Vec<String>,
}

impl OrderedPropagator {
    fn new() -> Self {
        let w3c = TraceContextPropagator::new();
        let b3 = opentelemetry_zipkin::Propagator::new();
        let baggage = BaggagePropagator::new();

        Self {
            fields: w3c
                .fields()
                .chain(b3.fields())
                .chain(baggage.fields())
                .map(|s| s.to_string())
                .collect(),
            w3c,
            b3,
            baggage,
        }
    }
}

impl TextMapPropagator for OrderedPropagator {
    fn inject_context(&self, cx: &Context, injector: &mut dyn Injector) {
        match cx.get::<PropagationFormat>() {
            None => {}
            Some(PropagationFormat::W3C) => {
                self.w3c.inject_context(cx, injector);
            }
            Some(PropagationFormat::B3) => {
                self.b3.inject_context(cx, injector);
            }
        }
        self.baggage.inject_context(cx, injector);
    }

    fn extract_with_context(&self, cx: &Context, extractor: &dyn Extractor) -> Context {
        let cx = if self.w3c.fields().any(|f| extractor.get(f).is_some()) {
            self.w3c
                .extract_with_context(cx, extractor)
                .with_value(PropagationFormat::W3C)
        } else if self.b3.fields().any(|f| extractor.get(f).is_some()) {
            self.b3
                .extract_with_context(cx, extractor)
                .with_value(PropagationFormat::B3)
        } else {
            cx.clone()
        };
        self.baggage.extract_with_context(&cx, extractor)
    }

    fn fields(&self) -> FieldIter<'_> {
        FieldIter::new(self.fields.as_slice())
    }
}
