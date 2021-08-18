mod config;
pub mod defaults;
mod discover;
mod store;
#[cfg(test)]
mod tests;

pub use self::config::Config;
pub(crate) use self::store::Store;
use linkerd_app_core::{
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Result,
};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;

pub(crate) trait CheckPolicy {
    /// Checks that the destination port is configured to allow traffic.
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(Arc<ServerPolicy>),
    Deny,
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
// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p.into())
    }
}

// === impl AllowPolicy ===

impl AllowPolicy {
    #[cfg(test)]
    pub(crate) fn for_test(dst: OrigDstAddr, server: ServerPolicy) -> Self {
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
