//! Core infrastructure for the proxy application.
//!
//! Conglomerates:
//! - Configuration
//! - Runtime initialization
//! - Admin interfaces
//! - Tap
//! - Metric labeling

#![deny(warnings, rust_2018_idioms)]

pub use linkerd_addr::{self as addr, Addr, NameAddr};
pub use linkerd_cache as cache;
pub use linkerd_conditional::Conditional;
pub use linkerd_detect as detect;
pub use linkerd_dns;
pub use linkerd_drain as drain;
pub use linkerd_error::{Error, Never, Recover};
pub use linkerd_exp_backoff as exp_backoff;
pub use linkerd_http_metrics as http_metrics;
pub use linkerd_identity as identity;
pub use linkerd_io as io;
pub use linkerd_opencensus as opencensus;
pub use linkerd_reconnect as reconnect;
pub use linkerd_service_profiles as profiles;
pub use linkerd_stack_metrics as stack_metrics;
pub use linkerd_stack_tracing as stack_tracing;
pub use linkerd_tls as tls;
pub use linkerd_tracing as trace;
pub use linkerd_transport_header as transport_header;

mod addr_match;
pub mod admin;
pub mod classify;
pub mod config;
pub mod control;
pub mod dns;
pub mod dst;
pub mod errors;
pub mod http_tracing;
pub mod metrics;
pub mod proxy;
pub mod retry;
pub mod serve;
pub mod svc;
pub mod telemetry;
pub mod transport;

pub use self::addr_match::{AddrMatch, IpMatch, NameMatch};

pub const CANONICAL_DST_HEADER: &str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &str = "l5d-dst-override";
pub const L5D_REQUIRE_ID: &str = "l5d-require-id";
pub const L5D_IDENTITY_HEADER: &str = "l5d-identity";

const DEFAULT_PORT: u16 = 80;

#[derive(Clone, Debug)]
pub struct ProxyRuntime {
    pub identity: Option<proxy::identity::LocalCrtKey>,
    pub metrics: metrics::Proxy,
    pub tap: proxy::tap::Registry,
    pub span_sink: http_tracing::OpenCensusSink,
    pub drain: drain::Watch,
}

pub fn discovery_rejected() -> tonic::Status {
    tonic::Status::new(tonic::Code::InvalidArgument, "Discovery rejected")
}

pub fn is_discovery_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(status) = err.downcast_ref::<tonic::Status>() {
        // Address is not resolveable
        status.code() == tonic::Code::InvalidArgument
            // Unexpected cluster state
            || status.code() == tonic::Code::FailedPrecondition
    } else if let Some(err) = err.source() {
        is_discovery_rejected(err)
    } else {
        false
    }
}

pub fn http_request_l5d_override_dst_name_addr<B>(
    req: &http::Request<B>,
) -> Result<NameAddr, addr::Error> {
    proxy::http::authority_from_header(req, DST_OVERRIDE_HEADER)
        .ok_or_else(|| {
            tracing::trace!("{} not in request headers", DST_OVERRIDE_HEADER);
            addr::Error::InvalidHost
        })
        .and_then(|a| NameAddr::from_authority_with_default_port(&a, DEFAULT_PORT))
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
