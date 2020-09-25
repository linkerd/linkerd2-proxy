//! Core infrastructure for the proxy application.
//!
//! Conglomerates:
//! - Configuration
//! - Runtime initialization
//! - Admin interfaces
//! - Tap
//! - Metric labeling

#![deny(warnings, rust_2018_idioms)]

pub use linkerd2_addr::{self as addr, Addr, NameAddr};
pub use linkerd2_admit as admit;
pub use linkerd2_cache as cache;
pub use linkerd2_conditional::Conditional;
pub use linkerd2_dns;
pub use linkerd2_drain as drain;
pub use linkerd2_error::{Error, Never, Recover};
pub use linkerd2_exp_backoff as exp_backoff;
pub use linkerd2_http_metrics as http_metrics;
pub use linkerd2_opencensus as opencensus;
pub use linkerd2_reconnect as reconnect;
pub use linkerd2_request_filter as request_filter;
pub use linkerd2_router as router;
pub use linkerd2_service_profiles as profiles;
pub use linkerd2_stack_metrics as stack_metrics;
pub use linkerd2_stack_tracing as stack_tracing;
pub use linkerd2_trace_context::TraceContextLayer;

pub mod admin;
pub mod classify;
pub mod config;
pub mod control;
pub mod dns;
pub mod dst;
pub mod errors;
pub mod handle_time;
pub mod metric_labels;
pub mod metrics;
pub mod proxy;
pub mod retry;
pub mod serve;
pub mod spans;
pub mod svc;
pub mod telemetry;
pub mod trace;
pub mod transport;

pub const CANONICAL_DST_HEADER: &'static str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &'static str = "l5d-dst-override";
pub const L5D_REQUIRE_ID: &'static str = "l5d-require-id";

const DEFAULT_PORT: u16 = 80;

pub fn http_request_l5d_override_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    proxy::http::authority_from_header(req, DST_OVERRIDE_HEADER)
        .ok_or_else(|| {
            tracing::trace!("{} not in request headers", DST_OVERRIDE_HEADER);
            addr::Error::InvalidHost
        })
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

pub fn http_request_authority_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    req.uri()
        .authority()
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(a, DEFAULT_PORT))
}

pub fn http_request_host_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::proxy::http::h1;

    h1::authority_from_host(req)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

pub type ControlHttpMetrics = http_metrics::Requests<metric_labels::ControlLabels, classify::Class>;

pub type HttpEndpointMetrics =
    http_metrics::Requests<metric_labels::EndpointLabels, classify::Class>;

pub type HttpRouteMetrics = http_metrics::Requests<metric_labels::RouteLabels, classify::Class>;

pub type HttpRouteRetry = http_metrics::Retries<metric_labels::RouteLabels>;

pub type StackMetrics = stack_metrics::Registry<metric_labels::StackLabels>;

#[derive(Clone)]
pub struct ProxyMetrics {
    pub http_handle_time: handle_time::Scope,
    pub http_route: HttpRouteMetrics,
    pub http_route_actual: HttpRouteMetrics,
    pub http_route_retry: HttpRouteRetry,
    pub http_endpoint: HttpEndpointMetrics,
    pub http_errors: errors::MetricsLayer,
    pub stack: StackMetrics,
    pub transport: transport::Metrics,
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryRejected(());

impl std::fmt::Display for DiscoveryRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "discovery rejected")
    }
}

impl std::error::Error for DiscoveryRejected {}

#[derive(Clone, Debug, Default)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl<T> linkerd2_stack::Switch<T> for SkipByPort
where
    for<'t> &'t T: Into<std::net::SocketAddr>,
{
    fn use_primary(&self, addrs: &T) -> bool {
        !self.0.contains(&addrs.into().port())
    }
}
