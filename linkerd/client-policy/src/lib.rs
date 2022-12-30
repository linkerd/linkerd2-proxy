#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_addr::{Addr, NameAddr};
use once_cell::sync::Lazy;
use std::{fmt, str::FromStr, sync::Arc};

pub mod http;
pub mod tcp;

/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Backend {
    pub addr: NameAddr,
    pub weight: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backends(Arc<[Backend]>);

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

// === impl Backend ===

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("addr", &format_args!("{}", self.addr))
            .field("weight", &self.weight)
            .finish()
    }
}

// === impl Backends ===

impl Backends {
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Backend> {
        self.0.iter()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Backends {
    fn default() -> Self {
        static NO_BACKENDS: Lazy<Backends> = Lazy::new(|| Backends(Arc::new([])));
        NO_BACKENDS.clone()
    }
}

impl FromIterator<Backend> for Backends {
    fn from_iter<I: IntoIterator<Item = Backend>>(iter: I) -> Self {
        let targets = iter.into_iter().collect::<Vec<_>>().into();
        Self(targets)
    }
}

impl AsRef<[Backend]> for Backends {
    #[inline]
    fn as_ref(&self) -> &[Backend] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Backends {
    type Item = &'a Backend;
    type IntoIter = std::slice::Iter<'a, Backend>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
