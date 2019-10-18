//! Core infrastructure for the proxy application.
//!
//! Conglomerates:
//! - Configuration
//! - Runtime initialization
//! - Admin interfaces
//! - Tap
//! - Metric labeling

#![warn(rust_2018_idioms)]

pub use linkerd2_addr::{self as addr, Addr, NameAddr};
pub use linkerd2_conditional::Conditional;
pub use linkerd2_dns as dns;
pub use linkerd2_drain as drain;
pub use linkerd2_error::{Error, Never, Recover};
pub use linkerd2_exp_backoff as exp_backoff;
pub use linkerd2_metrics as metrics;
pub use linkerd2_opencensus as opencensus;
pub use linkerd2_proxy_api_resolve as api_resolve;
pub use linkerd2_proxy_core as core;
pub use linkerd2_proxy_discover as discover;
pub use linkerd2_proxy_resolve as resolve;
pub use linkerd2_reconnect as reconnect;
pub use linkerd2_request_filter as request_filter;
pub use linkerd2_task as task;
pub use linkerd2_trace_context as trace_context;

pub mod accept_error;
pub mod admin;
pub mod classify;
pub mod config;
pub mod control;
pub mod dst;
pub mod errors;
pub mod handle_time;
pub mod identity;
pub mod metric_labels;
pub mod profiles;
pub mod proxy;
pub mod serve;
pub mod spans;
pub mod svc;
pub mod tap;
pub mod telemetry;
pub mod trace;
pub mod transport;

pub const CANONICAL_DST_HEADER: &'static str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &'static str = "l5d-dst-override";
pub const L5D_REMOTE_IP: &'static str = "l5d-remote-ip";
pub const L5D_SERVER_ID: &'static str = "l5d-server-id";
pub const L5D_CLIENT_ID: &'static str = "l5d-client-id";
pub const L5D_REQUIRE_ID: &'static str = "l5d-require-id";

pub fn init() -> Result<(config::Config, trace::LevelHandle), linkerd2_error::Error> {
    let trace_admin = trace::init()?;
    let cfg = config::Config::parse(&config::Env)?;
    Ok((cfg, trace_admin))
}

const DEFAULT_PORT: u16 = 80;

pub fn http_request_l5d_override_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    proxy::http::authority_from_header(req, DST_OVERRIDE_HEADER)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

pub fn http_request_authority_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    req.uri()
        .authority_part()
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(a, DEFAULT_PORT))
}

pub fn http_request_host_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::proxy::http::h1;

    h1::authority_from_host(req)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

pub fn http_request_orig_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::transport::Source;

    req.extensions()
        .get::<Source>()
        .and_then(|src| src.orig_dst_if_not_local())
        .map(Addr::Socket)
        .ok_or(addr::Error::InvalidHost)
}

#[derive(Copy, Clone, Debug)]
pub struct DispatchDeadline(std::time::Instant);

impl DispatchDeadline {
    pub fn after(allowance: std::time::Duration) -> DispatchDeadline {
        DispatchDeadline(tokio_timer::clock::now() + allowance)
    }

    pub fn extract<A>(req: &http::Request<A>) -> Option<std::time::Instant> {
        req.extensions().get::<DispatchDeadline>().map(|d| d.0)
    }
}

pub type HttpEndpointMetricsRegistry =
    linkerd2_proxy_http::metrics::SharedRegistry<metric_labels::EndpointLabels, classify::Class>;

pub type HttpRouteMetricsRegistry =
    linkerd2_proxy_http::metrics::SharedRegistry<metric_labels::RouteLabels, classify::Class>;
