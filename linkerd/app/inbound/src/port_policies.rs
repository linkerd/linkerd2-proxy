use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct PortPolicies {
    by_port: Arc<Map>,
    default: DefaultPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefaultPolicy {
    Allow(AllowPolicy),
    Deny,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AllowPolicy {
    /// Allows all connections with an authenticated client ID.
    Authenticated,
    /// Allows all terminated TLS connections, including those with no client ID.
    TlsUnauthenticated,
    /// Allows all unauthenticated connections.
    Unauthenticated,
}

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

type Map = HashMap<u16, AllowPolicy, BuildHasherDefault<PortHasher>>;

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub struct DeniedUnknownPort(u16);

// === impl PortPolicies ===

impl PortPolicies {
    pub fn check_allowed(&self, port: u16) -> Result<AllowPolicy, DeniedUnknownPort> {
        self.by_port
            .get(&port)
            .cloned()
            .map(Ok)
            .unwrap_or(match self.default {
                DefaultPolicy::Allow(a) => Ok(a),
                DefaultPolicy::Deny => Err(DeniedUnknownPort(port)),
            })
    }
}

impl From<AllowPolicy> for PortPolicies {
    fn from(default: AllowPolicy) -> Self {
        DefaultPolicy::Allow(default).into()
    }
}

impl From<DefaultPolicy> for PortPolicies {
    fn from(default: DefaultPolicy) -> Self {
        Self {
            default,
            by_port: Default::default(),
        }
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
