mod config;
pub mod defaults;
mod discover;
#[cfg(test)]
mod tests;

pub use self::config::Config;
use linkerd_app_core::{
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Result,
};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::{
    collections::{BTreeMap, HashMap},
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::watch;

pub(crate) trait CheckPolicy {
    /// Checks that the destination port is configured to allow traffic.
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort>;
}

pub type PortMap<T> = HashMap<u16, T, BuildHasherDefault<PortHasher>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(Arc<ServerPolicy>),
    Deny,
}

#[derive(Clone, Debug)]
pub(crate) struct PortPolicies {
    default: DefaultPolicy,
    ports: Arc<PortMap<watch::Receiver<Arc<ServerPolicy>>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AllowPolicy {
    dst: OrigDstAddr,
    server: Arc<ServerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Permitted {
    pub protocol: Protocol,
    pub tls: tls::ConditionalServerTls,

    // We want predictable ordering of labels, so we use a BTreeMap.
    pub labels: BTreeMap<String, String>,
}

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
pub struct PortHasher(u16);

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub(crate) struct DeniedUnknownPort(u16);

#[derive(Debug, Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub(crate) struct DeniedUnauthorized {
    client_addr: Remote<ClientAddr>,
    dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
}

// === impl PortPolicies ===

impl PortPolicies {
    #[cfg(test)]
    pub(crate) fn from_default(default: impl Into<DefaultPolicy>) -> Self {
        Self {
            default: default.into(),
            ports: Default::default(),
        }
    }
}

impl CheckPolicy for PortPolicies {
    /// Checks that the destination port is configured to allow traffic.
    ///
    /// If the port is not explicitly configured, then the default policy is used. If the default
    /// policy is `deny`, then a `DeniedUnknownPort` error is returned; otherwise an `AllowPolicy`
    /// is returned that can be used to check whether the connection is permitted via
    /// [`AllowPolicy::check_authorized`].
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        let server = self
            .ports
            .get(&dst.port())
            .map(|s| s.borrow().clone())
            .map(Ok)
            .unwrap_or(match &self.default {
                DefaultPolicy::Allow(a) => Ok(a.clone()),
                DefaultPolicy::Deny => Err(DeniedUnknownPort(dst.port())),
            })?;

        Ok(AllowPolicy { dst, server })
    }
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p.into())
    }
}

// === impl AllowPolicy ===

impl AllowPolicy {
    #[cfg(test)]
    pub(crate) fn new(dst: OrigDstAddr, server: ServerPolicy) -> Self {
        Self {
            dst,
            server: server.into(),
        }
    }

    pub(crate) fn is_opaque(&self) -> bool {
        self.server.protocol == Protocol::Opaque
    }

    /// Checks whether the destination port's `AllowPolicy` is authorized to accept connections
    /// given the provided TLS state.
    pub(crate) fn check_authorized(
        &self,
        client_addr: Remote<ClientAddr>,
        tls: tls::ConditionalServerTls,
    ) -> Result<Permitted, DeniedUnauthorized> {
        for authz in self.server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&client_addr.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => {
                        return Ok(Permitted::new(&self.server, authz, tls));
                    }

                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(Permitted::new(&self.server, authz, tls));
                        }
                    }

                    Authentication::TlsAuthenticated {
                        ref identities,
                        ref suffixes,
                    } => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            client_id: Some(tls::server::ClientId(ref id)),
                            ..
                        }) = tls
                        {
                            if identities.contains(id.as_ref())
                                || suffixes.iter().any(|s| s.contains(id.as_ref()))
                            {
                                return Ok(Permitted::new(&self.server, authz, tls));
                            }
                        }
                    }
                }
            }
        }

        Err(DeniedUnauthorized {
            client_addr,
            dst_addr: self.dst,
            tls,
        })
    }
}

// === impl Permitted ===

impl Permitted {
    fn new(server: &ServerPolicy, authz: &Authorization, tls: tls::ConditionalServerTls) -> Self {
        let mut labels = BTreeMap::new();
        labels.extend(server.labels.clone());
        labels.extend(authz.labels.clone());
        Self {
            protocol: server.protocol,
            labels,
            tls,
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
