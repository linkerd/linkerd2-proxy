use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
    str::FromStr,
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
    /// Allows all unauthenticated connections.
    Unauthenticated { skip_detect: bool },
    /// Allows all TLS connections (authenticated or otherwise), but denies
    /// non-TLS unauthenticated connections.
    TlsUnauthenticated,
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

#[derive(Clone, Debug, Error)]
#[error("expected one of `deny`, `authenticated`, `unauthenticated`, or `tls-unauthenticated`")]
pub struct ParsePolicyError(());

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

impl PortPolicies {
    pub fn new(default: DefaultPolicy, iter: impl IntoIterator<Item = (u16, AllowPolicy)>) -> Self {
        Self {
            default,
            by_port: Arc::new(iter.into_iter().collect::<Map>()),
        }
    }
}

impl From<DefaultPolicy> for PortPolicies {
    fn from(default: DefaultPolicy) -> Self {
        Self::new(default, None)
    }
}

impl From<AllowPolicy> for PortPolicies {
    fn from(default: AllowPolicy) -> Self {
        DefaultPolicy::from(default).into()
    }
}

// === impl DefaultPolicy ===

impl From<AllowPolicy> for DefaultPolicy {
    fn from(default: AllowPolicy) -> Self {
        DefaultPolicy::Allow(default)
    }
}

impl FromStr for DefaultPolicy {
    type Err = ParsePolicyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "deny" {
            return Ok(Self::Deny);
        }

        AllowPolicy::from_str(s).map(Self::Allow)
    }
}

// === impl AllowPolicy ===

impl FromStr for AllowPolicy {
    type Err = ParsePolicyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "authenticated" => Ok(Self::Authenticated),
            "unauthenticated" => Ok(Self::Unauthenticated { skip_detect: false }),
            "tls-unauthenticated" => Ok(Self::TlsUnauthenticated),
            _ => Err(ParsePolicyError(())),
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
