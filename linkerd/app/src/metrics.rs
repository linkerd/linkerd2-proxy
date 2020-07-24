pub use linkerd2_app_core::{
    classify::Class,
    errors, handle_time, http_metrics as metrics,
    metric_labels::{ControlLabels, EndpointLabels, RouteLabels},
    metrics::FmtMetrics,
    opencensus, proxy, stack_metrics, telemetry, transport, ControlHttpMetrics, ProxyMetrics,
};
use std::time::{Duration, SystemTime};

pub struct Metrics {
    pub inbound: ProxyMetrics,
    pub outbound: ProxyMetrics,
    pub control: ControlHttpMetrics,
    pub opencensus: opencensus::metrics::Registry,
}

impl Metrics {
    pub fn new(retain_idle: Duration) -> (Self, impl FmtMetrics + Clone + Send + 'static) {
        let process = telemetry::process::Report::new(SystemTime::now());

        let build_info = telemetry::build_info::Report::new();

        let (control, control_report) = {
            let m = metrics::Requests::<ControlLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("control");
            (m, r)
        };

        let (http_endpoint, endpoint_report) = {
            let m = metrics::Requests::<EndpointLabels, Class>::default();
            let r = m.clone().into_report(retain_idle);
            (m, r)
        };

        let (http_route, route_report) = {
            let m = metrics::Requests::<RouteLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_route_retry, retry_report) = {
            let m = metrics::Retries::<RouteLabels>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_route_actual, actual_report) = {
            let m = metrics::Requests::<RouteLabels, Class>::default();
            let r = m
                .clone()
                .into_report(retain_idle)
                .with_prefix("route_actual");
            (m, r.without_latencies())
        };

        let http_errors = errors::Metrics::default();

        let handle_time_report = handle_time::Metrics::new();
        let inbound_handle_time = handle_time_report.inbound();
        let outbound_handle_time = handle_time_report.outbound();

        let stack = stack_metrics::Registry::default();

        let (transport, transport_report) = transport::metrics::new();

        let (opencensus, opencensus_report) = opencensus::metrics::new();

        let metrics = Metrics {
            inbound: ProxyMetrics {
                http_handle_time: inbound_handle_time,
                http_endpoint: http_endpoint.clone(),
                http_route: http_route.clone(),
                http_route_actual: http_route_actual.clone(),
                http_route_retry: http_route_retry.clone(),
                http_errors: http_errors.inbound(),
                stack: stack.clone(),
                transport: transport.clone(),
            },
            outbound: ProxyMetrics {
                http_handle_time: outbound_handle_time,
                http_endpoint,
                http_route,
                http_route_retry,
                http_route_actual,
                http_errors: http_errors.outbound(),
                stack: stack.clone(),
                transport,
            },
            control,
            opencensus,
        };

        let report = (http_errors.report())
            .and_then(endpoint_report)
            .and_then(route_report)
            .and_then(retry_report)
            .and_then(actual_report)
            .and_then(control_report)
            .and_then(handle_time_report)
            .and_then(transport_report)
            .and_then(opencensus_report)
            .and_then(stack)
            .and_then(process)
            .and_then(build_info);

        (metrics, report)
    }
}
