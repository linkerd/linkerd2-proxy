pub use crate::exp_backoff::ExponentialBackoff;
use crate::{
    proxy::http::{h1, h2},
    svc::Param,
    transport::{Keepalive, ListenAddr},
};
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, Hasher},
    iter::FromIterator,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: ListenAddr,
    pub keepalive: Keepalive,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ConnectConfig {
    pub backoff: ExponentialBackoff,
    pub timeout: Duration,
    pub keepalive: Keepalive,
    pub h1_settings: h1::PoolSettings,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub connect: ConnectConfig,
    pub buffer_capacity: usize,
    pub cache_max_idle_age: Duration,
    pub dispatch_timeout: Duration,
    pub max_in_flight_requests: usize,
    pub detect_protocol_timeout: Duration,
}

/// A `HashSet` specialized for ports.
///
/// Because ports are `u16` values, this type avoids the overhead of actually
/// hashing ports.
#[derive(Clone, Debug, Default)]
pub struct PortSet(HashSet<u16, BuildHasherDefault<PortHasher>>);

/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u64);

// === impl ServerConfig ===

impl Param<ListenAddr> for ServerConfig {
    fn param(&self) -> ListenAddr {
        self.addr
    }
}

impl Param<Keepalive> for ServerConfig {
    fn param(&self) -> Keepalive {
        self.keepalive
    }
}

// === impl PortSet ===

impl PortSet {
    #[inline]
    pub fn contains(&self, port: u16) -> bool {
        self.0.contains(&port)
    }
}

impl FromIterator<u16> for PortSet {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = u16>,
    {
        Self(HashSet::from_iter(iter))
    }
}
// === impl PortHasher ===

impl Hasher for PortHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("hashing a `u16` calls `write_u16`");
    }

    #[inline]
    fn write_u16(&mut self, port: u16) {
        self.0 = port as u64;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
