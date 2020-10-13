use crate::{dns, identity::LocalIdentity};
use linkerd2_app_core::{control, metrics::ControlHttp as HttpMetrics, Error};
use linkerd2_opencensus::{metrics, proto, SpanExporter};
use std::future::Future;
use std::pin::Pin;
use std::{collections::HashMap, time::SystemTime};
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        control: control::Config,
        attributes: HashMap<String, String>,
        hostname: Option<String>,
    },
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub type SpanSink = mpsc::Sender<proto::trace::v1::Span>;

pub enum OcCollector {
    Disabled,
    Enabled {
        addr: control::ControlAddr,
        span_sink: SpanSink,
        task: Task,
    },
}

impl Config {
    const SPAN_BUFFER_CAPACITY: usize = 100;
    const SERVICE_NAME: &'static str = "linkerd-proxy";

    pub fn build(
        self,
        identity: LocalIdentity,
        dns: dns::Resolver,
        metrics: metrics::Registry,
        client_metrics: HttpMetrics,
    ) -> Result<OcCollector, Error> {
        match self {
            Config::Disabled => Ok(OcCollector::Disabled),
            Config::Enabled {
                control,
                hostname,
                attributes,
            } => {
                let addr = control.addr.clone();
                let svc = control.build(dns, client_metrics, identity);

                let (span_sink, spans_rx) = mpsc::channel(Self::SPAN_BUFFER_CAPACITY);

                let task = {
                    use self::proto::agent::common::v1 as oc;

                    let node = oc::Node {
                        identifier: Some(oc::ProcessIdentifier {
                            host_name: hostname.unwrap_or_default(),
                            pid: std::process::id(),
                            start_timestamp: Some(SystemTime::now().into()),
                        }),
                        service_info: Some(oc::ServiceInfo {
                            name: Self::SERVICE_NAME.to_string(),
                        }),
                        attributes,
                        ..oc::Node::default()
                    };

                    let addr = addr.clone();
                    Box::pin(async move {
                        debug!(peer.addr = ?addr, "running");
                        SpanExporter::new(svc, node, spans_rx, metrics).await
                    })
                };

                Ok(OcCollector::Enabled {
                    addr,
                    task,
                    span_sink,
                })
            }
        }
    }
}

impl OcCollector {
    pub fn span_sink(&self) -> Option<SpanSink> {
        match self {
            OcCollector::Disabled => None,
            OcCollector::Enabled { ref span_sink, .. } => Some(span_sink.clone()),
        }
    }
}
