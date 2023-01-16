#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd2_proxy_api as api;
use linkerd_proxy_core as core;

mod metadata;
pub mod pb;
mod resolve;

pub use self::metadata::{Metadata, ProtocolHint};
pub use self::resolve::Resolve;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DestinationGetPath(pub String);

impl std::fmt::Display for DestinationGetPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
