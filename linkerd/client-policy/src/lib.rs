#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod http;

#[cfg(feature = "proto")]
pub mod proto;

use linkerd_addr::{Addr, NameAddr};
// pub use linkerd_policy_core::{meta, Meta};
use linkerd_proxy_api_resolve as resolve;
use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc};
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
    pub addr: Option<LogicalAddr>,
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

/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

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
    pub fn logical_addr(&self) -> Option<LogicalAddr> {
        self.inner.borrow().addr.clone()
    }

    pub fn is_opaque_protocol(&self) -> bool {
        self.inner.borrow().opaque_protocol
    }

    pub fn endpoint(&self) -> Option<(SocketAddr, resolve::Metadata)> {
        self.inner.borrow().endpoint.clone()
    }
}

// === impl LookupAddr ===

impl fmt::Display for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LookupAddr({})", self.0)
    }
}

impl FromStr for LookupAddr {
    type Err = <Addr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Addr::from_str(s).map(LookupAddr)
    }
}

impl From<Addr> for LookupAddr {
    fn from(a: Addr) -> Self {
        Self(a)
    }
}

impl From<LookupAddr> for Addr {
    fn from(LookupAddr(addr): LookupAddr) -> Addr {
        addr
    }
}

// === impl LogicalAddr ===

impl fmt::Display for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalAddr({})", self.0)
    }
}

impl FromStr for LogicalAddr {
    type Err = <NameAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NameAddr::from_str(s).map(LogicalAddr)
    }
}

impl From<NameAddr> for LogicalAddr {
    fn from(na: NameAddr) -> Self {
        Self(na)
    }
}

impl From<LogicalAddr> for NameAddr {
    fn from(LogicalAddr(na): LogicalAddr) -> NameAddr {
        na
    }
}
