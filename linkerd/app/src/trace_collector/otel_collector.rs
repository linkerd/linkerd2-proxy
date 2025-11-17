use super::EnabledCollector;
use linkerd_app_core::{control::ControlAddr, proxy::http::Body, Error};
use linkerd_opentelemetry::{self as opentelemetry, metrics, otel::KeyValue, sdk, semconv};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
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
    let (span_sink, _) = mpsc::channel(crate::trace_collector::SPAN_BUFFER_CAPACITY);

    let resource = sdk::Resource::builder()
        .with_attribute(KeyValue::new(
            semconv::attribute::PROCESS_PID,
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
                .map(|(k, v)| KeyValue::new(k, v)),
        )
        .build();

    let addr = addr.clone();
    opentelemetry::install_opentelemetry_providers(svc, resource, legacy_metrics);

    EnabledCollector { addr, span_sink }
}
