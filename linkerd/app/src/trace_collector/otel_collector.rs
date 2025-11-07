use super::EnabledCollector;
use linkerd_app_core::{control::ControlAddr, proxy::http::Body, Error};
use linkerd_opentelemetry::otel::KeyValue;
use linkerd_opentelemetry::{self as opentelemetry, metrics, semconv};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tonic::{body::Body as TonicBody, client::GrpcService};

pub(super) struct OtelCollectorAttributes {
    pub hostname: Option<String>,
    pub extra: HashMap<String, String>,
}

pub(super) fn create_collector<S>(
    addr: ControlAddr,
    attributes: OtelCollectorAttributes,
    svc: S,
    legacy_metrics: metrics::Registry,
) -> EnabledCollector
where
    S: GrpcService<TonicBody> + Clone + Send + Sync + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
    S::ResponseBody: Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as Body>::Error: Into<Error> + Send,
{
    let resource = linkerd_opentelemetry::sdk::Resource::builder()
        .with_attribute(KeyValue::new(
            semconv::trace::PROCESS_PID,
            std::process::id() as i64,
        ))
        .with_attribute(KeyValue::new(
            "process.start_timestamp",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_else(|e| -(e.duration().as_secs() as i64)),
        ))
        .with_attribute(KeyValue::new(
            semconv::attribute::HOST_NAME,
            attributes.hostname.unwrap_or_default(),
        ))
        .with_attributes(
            attributes
                .extra
                .into_iter()
                .map(|(key, value)| KeyValue::new(key, value)),
        )
        .build();

    let addr = addr.clone();
    opentelemetry::setup_global_trace_exporter(svc, resource, legacy_metrics);

    EnabledCollector { addr }
}
