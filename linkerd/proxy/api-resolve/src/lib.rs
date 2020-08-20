#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "512"]

use linkerd2_identity as identity;
use linkerd2_proxy_api as api;
use linkerd2_proxy_core as core;

mod metadata;
mod pb;
mod resolve;

pub use self::metadata::{Metadata, ProtocolHint};
pub use self::resolve::Resolve;
