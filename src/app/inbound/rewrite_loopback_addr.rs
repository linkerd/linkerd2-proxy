//! Rewrites connect `SocketAddr`s IP address to the loopback address (`127.0.0.1`),
//! with the same port still set.

use super::Endpoint;
use crate::svc::stack::map_target;
use std::net::SocketAddr;
use tracing::debug;

pub fn layer() -> map_target::Layer<impl Fn(Endpoint) -> Endpoint + Copy> {
    map_target::layer(|mut ep: Endpoint| {
        debug!("rewriting inbound address to loopback; addr={:?}", ep.addr);
        ep.addr = SocketAddr::from(([127, 0, 0, 1], ep.addr.port()));
        ep
    })
}
