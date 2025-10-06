use linkerd_app_core::{
    control, dns, http_tracing::SpanSink, identity, metrics::ControlHttp as HttpMetrics,
    opentelemetry, svc::NewService,
};
use linkerd_error::Error;
use otel_collector::OtelCollectorAttributes;
use std::{collections::HashMap, future::Future, pin::Pin};

pub mod otel_collector;

const SPAN_BUFFER_CAPACITY: usize = 100;

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
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub enum TraceCollector {
    Disabled,
    Enabled(Box<EnabledCollector>),
}

pub struct EnabledCollector {
    pub addr: control::ControlAddr,
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
            Config::Enabled(_) => Some("opentelemetry"),
        }
    }

    pub fn build(
        self,
        identity: identity::NewClient,
        dns: dns::Resolver,
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

                let collector = {
                    let attributes = OtelCollectorAttributes {
                        hostname: inner.hostname,
                        extra: inner.attributes,
                    };
                    otel_collector::create_collector(
                        addr.clone(),
                        attributes,
                        svc,
                        legacy_otel_metrics,
                    )
                };

                Ok(TraceCollector::Enabled(Box::new(collector)))
            }
        }
    }
}
