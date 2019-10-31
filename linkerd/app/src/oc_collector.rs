use crate::{dns, identity::LocalIdentity};
use futures::Future;
use linkerd2_app_core::{
    config::ControlConfig,
    control, proxy, reconnect,
    svc::{self, LayerExt},
    transport::{connect, tls},
    Error,
};
use linkerd2_opencensus::{metrics, proto, SpanExporter};
use std::time::SystemTime;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        control: ControlConfig,
        hostname: Option<String>,
    },
}

pub type Task = Box<dyn Future<Item = (), Error = Error> + Send + 'static>;

pub type SpanSink = mpsc::Sender<proto::trace::v1::Span>;

pub enum OcCollector {
    Disabled,
    Enabled { span_sink: SpanSink, task: Task },
}

impl Config {
    const SPAN_BUFFER_CAPACITY: usize = 100;
    const SERVICE_NAME: &'static str = "linkerd-proxy";

    pub fn build(
        self,
        identity: LocalIdentity,
        dns: dns::Resolver,
        metrics: metrics::Registry,
    ) -> Result<OcCollector, Error> {
        match self {
            Config::Disabled => Ok(OcCollector::Disabled),
            Config::Enabled { control, hostname } => {
                let svc = svc::stack(connect::svc(control.connect.keepalive))
                    .push(tls::client::layer(identity))
                    .push_timeout(control.connect.timeout)
                    // TODO: perhaps rename from "control" to "grpc"
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns.clone()))
                    // TODO: we should have metrics of some kind, but the standard
                    // HTTP metrics aren't useful for a client where we never read
                    // the response.
                    .push(reconnect::layer({
                        let backoff = control.connect.backoff;
                        move |_| Ok(backoff.stream())
                    }))
                    .push(proxy::grpc::req_body_as_payload::layer().per_make())
                    .push(control::add_origin::layer())
                    .push_buffer_pending(
                        control.buffer.max_in_flight,
                        control.buffer.dispatch_timeout,
                    )
                    .into_inner()
                    .make(control.addr);

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
                        ..oc::Node::default()
                    };
                    Box::new(SpanExporter::new(svc, node, spans_rx, metrics))
                };

                Ok(OcCollector::Enabled { task, span_sink })
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
