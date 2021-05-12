#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]
#![recursion_limit = "512"]

use linkerd2_proxy_api as api;
use linkerd_addr::NameAddr;
use linkerd_proxy_core as core;

mod metadata;
pub mod pb;
mod resolve;

pub use self::metadata::{Metadata, ProtocolHint};
pub use self::resolve::Resolve;

// TODO this should hold a `NameAddr`; but this currently isn't possible due to
// outbound target types.
#[derive(Clone, Debug)]
pub struct ConcreteAddr(pub NameAddr);

impl std::fmt::Display for ConcreteAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
