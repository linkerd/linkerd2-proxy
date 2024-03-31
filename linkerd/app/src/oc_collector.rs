use linkerd_app_core::{
    control, dns, identity, metrics::ControlHttp as HttpMetrics, svc::NewService, Error,
};
use linkerd_opencensus::{self as opencensus, metrics, proto};
use std::{collections::HashMap, future::Future, pin::Pin, time::SystemTime};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::Instrument;

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
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub type SpanSink = mpsc::Sender<proto::trace::v1::Span>;

pub enum OcCollector {
    Disabled,
    Enabled(Box<EnabledCollector>),
}

pub struct EnabledCollector {
    pub addr: control::ControlAddr,
    pub span_sink: SpanSink,
    pub task: Task,
}

impl Config {
    const SPAN_BUFFER_CAPACITY: usize = 100;
    const SERVICE_NAME: &'static str = "linkerd-proxy";

    pub fn build(
        self,
        identity: identity::NewClient,
        dns: dns::Resolver,
        legacy_metrics: metrics::Registry,
        control_metrics: control::Metrics,
        client_metrics: HttpMetrics,
    ) -> Result<OcCollector, Error> {
        match self {
            Config::Disabled => Ok(OcCollector::Disabled),
            Config::Enabled(inner) => {
                let addr = inner.control.addr.clone();
                let svc = inner
                    .control
                    .build(dns, client_metrics, control_metrics, identity)
                    .new_service(());

                let (span_sink, spans_rx) = mpsc::channel(Self::SPAN_BUFFER_CAPACITY);
                let spans_rx = ReceiverStream::new(spans_rx);

                let task = {
                    use self::proto::agent::common::v1 as oc;

                    let node = oc::Node {
                        identifier: Some(oc::ProcessIdentifier {
                            host_name: inner.hostname.unwrap_or_default(),
                            pid: std::process::id(),
                            start_timestamp: Some(SystemTime::now().into()),
                        }),
                        service_info: Some(oc::ServiceInfo {
                            name: inner.service_name.unwrap_or(Self::SERVICE_NAME.to_string()),
                        }),
                        attributes: inner.attributes,
                        ..oc::Node::default()
                    };

                    let addr = addr.clone();
                    Box::pin(
                        opencensus::export_spans(svc, node, spans_rx, legacy_metrics).instrument(
                            tracing::debug_span!("opencensus", peer.addr = %addr).or_current(),
                        ),
                    )
                };

                Ok(OcCollector::Enabled(Box::new(EnabledCollector {
                    addr,
                    task,
                    span_sink,
                })))
            }
        }
    }
}

impl OcCollector {
    pub fn span_sink(&self) -> Option<SpanSink> {
        match self {
            OcCollector::Disabled => None,
            OcCollector::Enabled(inner) => Some(inner.span_sink.clone()),
        }
    }
}
