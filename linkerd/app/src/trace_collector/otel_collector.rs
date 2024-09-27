use super::EnabledCollector;
use linkerd_app_core::{
    control::ControlAddr, http_tracing::CollectorProtocol, proxy::http::HttpBody, Error,
};
use linkerd_opentelemetry::{
    self as opentelemetry, metrics,
    proto::proto::common::v1::{any_value, AnyValue, KeyValue},
    proto::transform::common::ResourceAttributesWithSchema,
};
use std::{collections::HashMap, time::SystemTime, time::UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{body::BoxBody, client::GrpcService};
use tracing::Instrument;

pub(super) fn create_collector<S>(
    addr: ControlAddr,
    hostname: Option<String>,
    attributes: HashMap<String, String>,
    svc: S,
    legacy_metrics: metrics::Registry,
) -> EnabledCollector
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
    S::ResponseBody: Default + HttpBody<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as HttpBody>::Error: Into<Error> + Send,
{
    let (span_sink, spans_rx) = mpsc::channel(crate::trace_collector::SPAN_BUFFER_CAPACITY);
    let spans_rx = ReceiverStream::new(spans_rx);

    let mut resources = ResourceAttributesWithSchema::default();

    resources
        .attributes
        .0
        .push(crate::trace_collector::SERVICE_NAME.with_key("service.name"));
    resources
        .attributes
        .0
        .push((std::process::id() as i64).with_key("process.pid"));

    resources.attributes.0.push(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(|e| -(e.duration().as_secs() as i64))
            .with_key("process.start_timestamp"),
    );
    resources
        .attributes
        .0
        .push(hostname.unwrap_or_default().with_key("host.name"));

    resources.attributes.0.extend(
        attributes
            .into_iter()
            .map(|(key, value)| value.with_key(&key)),
    );

    let addr = addr.clone();
    let task = Box::pin(
        opentelemetry::export_spans(svc, spans_rx, resources, legacy_metrics)
            .instrument(tracing::debug_span!("opentelemetry", peer.addr = %addr).or_current()),
    );

    EnabledCollector {
        addr,
        task,
        span_sink,
        kind: CollectorProtocol::OpenTelemetry,
    }
}

trait IntoAnyValue
where
    Self: Sized,
{
    fn into_any_value(self) -> AnyValue;

    fn with_key(self, key: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(self.into_any_value()),
        }
    }
}

impl IntoAnyValue for String {
    fn into_any_value(self) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::StringValue(self)),
        }
    }
}

impl IntoAnyValue for &str {
    fn into_any_value(self) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::StringValue(self.to_string())),
        }
    }
}

impl IntoAnyValue for i64 {
    fn into_any_value(self) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::IntValue(self)),
        }
    }
}
