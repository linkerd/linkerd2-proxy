use crate::trace_collector::EnabledCollector;
use linkerd_app_core::{
    control::ControlAddr, http_tracing::CollectorProtocol, proxy::http::HttpBody, Error,
};
use linkerd_opencensus::{self as opencensus, metrics, proto};
use std::{collections::HashMap, time::SystemTime};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{body::BoxBody, client::GrpcService};
use tracing::Instrument;

pub(super) fn create_collector<S>(
    addr: ControlAddr,
    hostname: Option<String>,
    service_name: String,
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

    let task = {
        use self::proto::agent::common::v1 as oc;

        let node = oc::Node {
            identifier: Some(oc::ProcessIdentifier {
                host_name: hostname.unwrap_or_default(),
                pid: std::process::id(),
                start_timestamp: Some(SystemTime::now().into()),
            }),
            service_info: Some(oc::ServiceInfo { name: service_name }),
            attributes,
            ..oc::Node::default()
        };

        let addr = addr.clone();
        Box::pin(
            opencensus::export_spans(svc, node, spans_rx, legacy_metrics)
                .instrument(tracing::debug_span!("opencensus", peer.addr = %addr).or_current()),
        )
    };

    EnabledCollector {
        addr,
        task,
        span_sink,
        kind: CollectorProtocol::OpenCensus,
    }
}
