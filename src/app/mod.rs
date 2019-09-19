//! Configures and runs the linkerd2 service sidecar proxy

mod admin;
mod classify;
pub mod config;
mod control;
mod dst;
mod errors;
mod handle_time;
mod identity;
mod inbound;
mod main;
mod metric_labels;
mod outbound;
mod profiles;
mod proxy;
mod spans;
mod tap;

pub use self::main::Main;
use crate::addr::{self, Addr};
use crate::logging::trace;
use http;
use std::error::Error;

const CANONICAL_DST_HEADER: &'static str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &'static str = "l5d-dst-override";
const L5D_REMOTE_IP: &'static str = "l5d-remote-ip";
const L5D_SERVER_ID: &'static str = "l5d-server-id";
const L5D_CLIENT_ID: &'static str = "l5d-client-id";
pub const L5D_REQUIRE_ID: &'static str = "l5d-require-id";

pub fn init() -> Result<(config::Config, trace::LevelHandle), Box<dyn Error + Send + Sync + 'static>>
{
    let trace_admin = trace::init()?;
    let cfg = config::Config::parse(&config::Env)?;
    Ok((cfg, trace_admin))
}

const DEFAULT_PORT: u16 = 80;

fn http_request_l5d_override_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::proxy;

    proxy::http::authority_from_header(req, DST_OVERRIDE_HEADER)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

fn http_request_authority_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    req.uri()
        .authority_part()
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(a, DEFAULT_PORT))
}

fn http_request_host_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::proxy::http::h1;

    h1::authority_from_host(req)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

fn http_request_orig_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use crate::proxy::Source;

    req.extensions()
        .get::<Source>()
        .and_then(|src| src.orig_dst_if_not_local())
        .map(Addr::Socket)
        .ok_or(addr::Error::InvalidHost)
}

#[derive(Copy, Clone, Debug)]
struct DispatchDeadline(std::time::Instant);

impl DispatchDeadline {
    fn after(allowance: std::time::Duration) -> DispatchDeadline {
        DispatchDeadline(tokio_timer::clock::now() + allowance)
    }

    fn extract<A>(req: &http::Request<A>) -> Option<std::time::Instant> {
        req.extensions().get::<DispatchDeadline>().map(|d| d.0)
    }
}

type HttpEndpointMetricsRegistry =
    crate::proxy::http::metrics::SharedRegistry<metric_labels::EndpointLabels, classify::Class>;
type HttpRouteMetricsRegistry =
    crate::proxy::http::metrics::SharedRegistry<metric_labels::RouteLabels, classify::Class>;
