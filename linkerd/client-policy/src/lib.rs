#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod http;

#[cfg(feature = "proto")]
pub mod proto;

use linkerd_addr::{Addr, NameAddr};
// pub use linkerd_policy_core::{meta, Meta};
use linkerd_proxy_api_resolve as resolve;
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::watch;

type Backends = Arc<[Backend]>;

#[derive(Clone, Debug)]
pub struct Receiver {
    inner: watch::Receiver<Policy>,
}

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

// === impl Receiver ===

impl From<watch::Receiver<Policy>> for Receiver {
    fn from(inner: watch::Receiver<Policy>) -> Self {
        Self { inner }
    }
}

impl From<Receiver> for watch::Receiver<Policy> {
    fn from(r: Receiver) -> watch::Receiver<Policy> {
        r.inner
    }
}

impl Receiver {
    pub fn logical_addr(&self) -> Option<NameAddr> {
        self.inner.borrow().addr.clone()
    }

    pub fn is_opaque_protocol(&self) -> bool {
        self.inner.borrow().opaque_protocol
    }

    pub fn endpoint(&self) -> Option<(SocketAddr, resolve::Metadata)> {
        self.inner.borrow().endpoint.clone()
    }
}
