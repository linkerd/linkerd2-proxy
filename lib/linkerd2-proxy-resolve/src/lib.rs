use linkerd2_identity as identity;
use linkerd2_proxy_api as api;
use linkerd2_proxy_core as core;

mod destination;
mod metadata;
mod pb;
mod remote_stream;

pub use self::destination::Resolve;
pub use self::metadata::{Metadata, ProtocolHint};
