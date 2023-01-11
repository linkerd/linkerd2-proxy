#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod http;

#[cfg(feature = "proto")]
pub mod proto;

use linkerd_addr::{Addr, NameAddr};
// pub use linkerd_policy_core::{meta, Meta};
use linkerd_proxy_api_resolve as resolve;
use std::{net::SocketAddr, sync::Arc};
type Backends = Arc<[Backend]>;

// TODO(eliza): this is looking a lot like the `Profile` type...could be made
// generic eventually...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub addr: Option<NameAddr>,
    pub http_routes: Arc<[http::Route]>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, resolve::Metadata)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy {
    pub backends: Backends,
    // pub meta: Arc<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    pub weight: u32,
    pub addr: Addr,
}
