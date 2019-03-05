//! Configures and runs the linkerd2 service sidecar proxy

use http;

mod classify;
pub mod config;
mod control;
mod dst;
mod inbound;
mod main;
mod metric_labels;
mod outbound;
mod profiles;

pub use self::main::Main;
use addr::{self, Addr};

const CANONICAL_DST_HEADER: &'static str = "l5d-dst-canonical";
pub const DST_OVERRIDE_HEADER: &'static str = "l5d-dst-override";
const L5D_REMOTE_IP: &'static str = "l5d-remote-ip";
const L5D_SERVER_ID: &'static str = "l5d-server-id";
const L5D_CLIENT_ID: &'static str = "l5d-client-id";

pub fn init() -> Result<config::Config, config::Error> {
    use convert::TryFrom;
    use logging;

    logging::init();
    config::Config::try_from(&config::Env)
}

const DEFAULT_PORT: u16 = 80;

fn http_request_l5d_override_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use proxy;

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
    use proxy::http::h1;

    h1::authority_from_host(req)
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
}

fn http_request_orig_dst_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use proxy::Source;

    req.extensions()
        .get::<Source>()
        .and_then(|src| src.orig_dst_if_not_local())
        .map(Addr::Socket)
        .ok_or(addr::Error::InvalidHost)
}
