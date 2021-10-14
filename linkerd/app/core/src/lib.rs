//! Core infrastructure for the proxy application.
//!
//! Conglomerates:
//! - Configuration
//! - Runtime initialization
//! - Admin interfaces
//! - Tap
//! - Metric labeling

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub use drain;
pub use ipnet::{IpNet, Ipv4Net, Ipv6Net};
pub use linkerd_addr::{self as addr, Addr, NameAddr};
pub use linkerd_cache as cache;
pub use linkerd_conditional::Conditional;
pub use linkerd_detect as detect;
pub use linkerd_dns;
pub use linkerd_error::{is_error, Error, Infallible, Recover, Result};
pub use linkerd_exp_backoff as exp_backoff;
pub use linkerd_http_metrics as http_metrics;
pub use linkerd_identity as identity;
pub use linkerd_io as io;
pub use linkerd_opencensus as opencensus;
pub use linkerd_service_profiles as profiles;
pub use linkerd_stack_metrics as stack_metrics;
pub use linkerd_stack_tracing as stack_tracing;
pub use linkerd_tls as tls;
pub use linkerd_tracing as trace;
pub use linkerd_transport_header as transport_header;

use thiserror::Error;

mod addr_match;
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

const DEFAULT_PORT: u16 = 80;

#[derive(Clone, Debug)]
pub struct ProxyRuntime {
    pub identity: proxy::identity::LocalCrtKey,
    pub metrics: metrics::Proxy,
    pub tap: proxy::tap::Registry,
    pub span_sink: http_tracing::OpenCensusSink,
    pub drain: drain::Watch,
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
