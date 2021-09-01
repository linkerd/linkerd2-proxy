mod authorize;
mod config;
pub mod defaults;
mod discover;
mod store;
#[cfg(test)]
mod tests;

pub use self::authorize::{NewAuthorizeHttp, NewAuthorizeTcp};
pub use self::config::Config;
pub(crate) use self::store::Store;

use linkerd_app_core::{
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Result,
};
pub use linkerd_server_policy::{
    Authentication, Authorization, Labels, Protocol, ServerPolicy, Suffix,
};
use thiserror::Error;
use tokio::sync::watch;

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub(crate) struct DeniedUnknownPort(pub u16);

#[derive(Debug, Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub(crate) struct DeniedUnauthorized {
    pub client_addr: Remote<ClientAddr>,
    pub dst_addr: OrigDstAddr,
    pub tls: tls::ConditionalServerTls,
}

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
pub struct AllowPolicy {
    dst: OrigDstAddr,
    server: watch::Receiver<ServerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Permit {
    pub protocol: Protocol,

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
    #[cfg(any(test, fuzzing))]
    pub(crate) fn for_test(
        dst: OrigDstAddr,
        server: ServerPolicy,
    ) -> (Self, watch::Sender<ServerPolicy>) {
        let (tx, server) = watch::channel(server);
        let p = Self { dst, server };
        (p, tx)
    }

    #[inline]
    pub(crate) fn protocol(&self) -> Protocol {
        self.server.borrow().protocol
    }

    #[inline]
    pub(crate) fn server_labels(&self) -> Labels {
        self.server.borrow().labels.clone()
    }

    /// Checks whether the destination port's `AllowPolicy` is authorized to accept connections
    /// given the provided TLS state.
    pub(crate) fn check_authorized(
        &self,
        client_addr: Remote<ClientAddr>,
        tls: &tls::ConditionalServerTls,
    ) -> Result<Permit, DeniedUnauthorized> {
        let server = self.server.borrow();
        for authz in server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&client_addr.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => {
                        return Ok(Permit::new(&*server, authz));
                    }

                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(Permit::new(&*server, authz));
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
                                return Ok(Permit::new(&*server, authz));
                            }
                        }
                    }
                }
            }
        }

        Err(DeniedUnauthorized {
            client_addr,
            dst_addr: self.dst,
            tls: tls.clone(),
        })
    }
}

// === impl Permit ===

impl Permit {
    fn new(server: &ServerPolicy, authz: &Authorization) -> Self {
        Self {
            protocol: server.protocol,
            server_labels: server.labels.clone(),
            authz_labels: authz.labels.clone(),
        }
    }
}
