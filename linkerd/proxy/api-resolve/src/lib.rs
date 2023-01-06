#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd2_proxy_api as api;
use linkerd_addr::NameAddr;
use linkerd_proxy_core as core;

mod metadata;
pub mod pb;
mod resolve;

pub use self::metadata::{Metadata, ProtocolHint};
pub use self::resolve::Resolve;

// TODO(ver) this should hold a structured address reference and not just a FQDN:port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConcreteAddr(pub NameAddr);

impl std::fmt::Display for ConcreteAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
