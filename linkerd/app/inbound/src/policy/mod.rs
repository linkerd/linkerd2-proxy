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
    transport::{ClientAddr, DeniedUnauthorized, DeniedUnknownPort, OrigDstAddr, Remote},
    Result,
};
pub use linkerd_server_policy::{
    Authentication, Authorization, Labels, Protocol, ServerPolicy, Suffix,
};
use std::sync::Arc;
use tokio::sync::watch;

pub(crate) trait CheckPolicy {
    /// Checks that the destination address is configured to allow traffic.
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(ServerPolicy),
    Deny,
}

#[derive(Clone, Debug)]
pub(crate) struct AllowPolicy {
    dst: OrigDstAddr,
    server: watch::Receiver<Arc<ServerPolicy>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Permit {
    pub protocol: Protocol,
    pub tls: tls::ConditionalServerTls,

    pub server_labels: Labels,
    pub authz_labels: Labels,
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p)
    }
}

// === impl AllowPolicy ===

impl AllowPolicy {
    #[cfg(test)]
    pub(crate) fn for_test(
        dst: OrigDstAddr,
        server: ServerPolicy,
    ) -> (Self, watch::Sender<Arc<ServerPolicy>>) {
        let (tx, server) = watch::channel(Arc::new(server));
        let p = Self { dst, server };
        (p, tx)
    }

    #[inline]
    pub(crate) fn protocol(&self) -> Protocol {
        self.server.borrow().protocol
    }

    /// Checks whether the destination port's `AllowPolicy` is authorized to accept connections
    /// given the provided TLS state.
    pub(crate) fn check_authorized(
        &self,
        client_addr: Remote<ClientAddr>,
        tls: tls::ConditionalServerTls,
    ) -> Result<Permit, DeniedUnauthorized> {
        let server = self.server.borrow();
        for authz in server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&client_addr.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => {
                        return Ok(Permit::new(&**self.server.borrow(), authz, tls));
                    }

                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(Permit::new(&**self.server.borrow(), authz, tls));
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
                                return Ok(Permit::new(&**self.server.borrow(), authz, tls));
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

// === impl Permit ===

impl Permit {
    fn new(server: &ServerPolicy, authz: &Authorization, tls: tls::ConditionalServerTls) -> Self {
        Self {
            protocol: server.protocol,
            server_labels: server.labels.clone(),
            authz_labels: authz.labels.clone(),
            tls,
        }
    }
}
