use linkerd_app_core::{
    control, dns,
    http_tracing::{CollectorProtocol, SpanSink},
    identity,
    metrics::ControlHttp as HttpMetrics,
    opencensus, opentelemetry,
    svc::NewService,
};
use linkerd_error::Error;
use otel_collector::OtelCollectorAttributes;
use std::{collections::HashMap, future::Future, pin::Pin};

pub mod oc_collector;
pub mod otel_collector;

const SPAN_BUFFER_CAPACITY: usize = 100;
const SERVICE_NAME: &str = "linkerd-proxy";

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled(Box<EnabledConfig>),
}

#[derive(Clone, Debug)]
pub struct EnabledConfig {
    pub control: control::Config,
    pub attributes: HashMap<String, String>,
    pub hostname: Option<String>,
    pub service_name: Option<String>,
    pub kind: CollectorProtocol,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub enum TraceCollector {
    Disabled,
    Enabled(Box<EnabledCollector>),
}

pub struct EnabledCollector {
    pub addr: control::ControlAddr,
    pub kind: CollectorProtocol,
    pub span_sink: SpanSink,
    pub task: Task,
}

impl TraceCollector {
    pub fn span_sink(&self) -> Option<SpanSink> {
        match self {
            TraceCollector::Disabled => None,
            TraceCollector::Enabled(inner) => Some(inner.span_sink.clone()),
        }
    }
}

impl Config {
    pub fn metrics_prefix(&self) -> Option<&'static str> {
        match self {
            Config::Disabled => None,
            Config::Enabled(config) => match config.kind {
                CollectorProtocol::OpenCensus => Some("opencensus"),
                CollectorProtocol::OpenTelemetry => Some("opentelemetry"),
            },
        }
    }

    pub fn build(
        self,
        identity: identity::NewClient,
        dns: dns::Resolver,
        legacy_oc_metrics: opencensus::metrics::Registry,
        legacy_otel_metrics: opentelemetry::metrics::Registry,
        control_metrics: control::Metrics,
        client_metrics: HttpMetrics,
    ) -> Result<TraceCollector, Error> {
        match self {
            Config::Disabled => Ok(TraceCollector::Disabled),
            Config::Enabled(inner) => {
                let addr = inner.control.addr.clone();
                let svc = inner
                    .control
                    .build(dns, client_metrics, control_metrics, identity)
                    .new_service(());
                let svc_name = inner
                    .service_name
                    .unwrap_or_else(|| SERVICE_NAME.to_string());

                let collector = match inner.kind {
                    CollectorProtocol::OpenCensus => oc_collector::create_collector(
                        addr.clone(),
                        inner.hostname,
                        svc_name,
                        inner.attributes,
                        svc,
                        legacy_oc_metrics,
                    ),
                    CollectorProtocol::OpenTelemetry => {
                        let attributes = OtelCollectorAttributes {
                            hostname: inner.hostname,
                            service_name: svc_name,
                            extra: inner.attributes,
                        };
                        otel_collector::create_collector(
                            addr.clone(),
                            attributes,
                            svc,
                            legacy_otel_metrics,
                        )
                    }
                };

                Ok(TraceCollector::Enabled(Box::new(collector)))
            }
        }
    }
}
