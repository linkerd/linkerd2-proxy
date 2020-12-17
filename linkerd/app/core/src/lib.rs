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
pub use linkerd2_cache as cache;
pub use linkerd2_conditional::Conditional;
pub use linkerd2_connection_header as connection_header;
pub use linkerd2_dns;
pub use linkerd2_drain as drain;
pub use linkerd2_error::{Error, Never, Recover};
pub use linkerd2_exp_backoff as exp_backoff;
pub use linkerd2_http_metrics as http_metrics;
pub use linkerd2_opencensus as opencensus;
pub use linkerd2_reconnect as reconnect;
pub use linkerd2_service_profiles as profiles;
pub use linkerd2_stack_metrics as stack_metrics;
pub use linkerd2_stack_tracing as stack_tracing;
pub use linkerd2_trace_context::TraceContext;
pub use linkerd2_tracing as trace;

mod addr_match;
pub mod admin;
pub mod classify;
pub mod config;
pub mod control;
pub mod dns;
pub mod dst;
pub mod errors;
pub mod handle_time;
pub mod metrics;
pub mod proxy;
pub mod retry;
pub mod serve;
pub mod spans;
pub mod svc;
pub mod telemetry;
pub mod transport;

pub use self::addr_match::{AddrMatch, IpMatch, NameMatch};

pub const CANONICAL_DST_HEADER: &str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &str = "l5d-dst-override";
pub const L5D_REQUIRE_ID: &str = "l5d-require-id";

const DEFAULT_PORT: u16 = 80;

pub fn discovery_rejected() -> tonic::Status {
    tonic::Status::new(tonic::Code::InvalidArgument, "Discovery rejected")
}

pub fn is_discovery_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(status) = err.downcast_ref::<tonic::Status>() {
        status.code() == tonic::Code::InvalidArgument
    } else if let Some(err) = err.source() {
        is_discovery_rejected(err)
    } else {
        false
    }
}

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
