pub use crate::exp_backoff::ExponentialBackoff;
use crate::{
    proxy::http::{self, h1, h2},
    svc::Param,
    transport::{Keepalive, ListenAddr},
};
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, Hasher},
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
pub type PortSet = HashSet<u16, BuildHasherDefault<PortHasher>>;

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
pub struct PortHasher(u16);

// === impl ProxyConfig ===

impl ProxyConfig {
    pub fn detect_http(&self) -> linkerd_detect::Config<http::DetectHttp> {
        linkerd_detect::Config::from_timeout(self.detect_protocol_timeout)
    }
}

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

// === impl PortHasher ===

impl Hasher for PortHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("hashing a `u16` calls `write_u16`");
    }

    #[inline]
    fn write_u16(&mut self, port: u16) {
        self.0 = port;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::*;

    quickcheck! {
        fn portset_contains_all_ports(ports: Vec<u16>) -> bool {
            // Make a port set containing the generated port numbers.
            let portset = ports.iter().cloned().collect::<PortSet>();
            for port in ports {
                // If the port set doesn't contain one of the ports it was
                // created with, that's bad news!
                if !portset.contains(&port) {
                    return false;
                }
            }

            true
        }
    }
}
