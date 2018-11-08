//! Configures and runs the linkerd2 service sidecar proxy

use http;

mod classify;
pub mod config;
mod control;
mod inbound;
mod main;
mod metric_labels;
mod outbound;
mod profiles;

pub use self::main::Main;
use addr::{self, Addr};

pub fn init() -> Result<config::Config, config::Error> {
    use convert::TryFrom;
    use logging;

    logging::init();
    config::Config::try_from(&config::Env)
}

fn http_request_addr<B>(req: &http::Request<B>) -> Result<Addr, addr::Error> {
    use proxy::{http::h1, Source};
    const DEFAULT_PORT: u16 = 80;

    req.uri()
        .authority_part()
        .ok_or(addr::Error::InvalidHost)
        .and_then(|a| Addr::from_authority_and_default_port(a, DEFAULT_PORT))
        .or_else(|_| {
            h1::authority_from_host(req)
                .ok_or(addr::Error::InvalidHost)
                .and_then(|a| Addr::from_authority_and_default_port(&a, DEFAULT_PORT))
        })
        .or_else(|e| {
            req.extensions()
                .get::<Source>()
                .and_then(|src| src.orig_dst_if_not_local())
                .map(Addr::Socket)
                .ok_or(e)
        })
}
