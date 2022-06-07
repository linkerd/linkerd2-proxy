mod api;
mod authorize;
mod config;
pub mod defaults;
mod store;
#[cfg(test)]
mod tests;

pub use self::authorize::{NewAuthorizeHttp, NewAuthorizeTcp};
pub use self::config::Config;
pub(crate) use self::store::Store;

use linkerd_app_core::metrics::ServerAuthzLabels;
pub use linkerd_app_core::metrics::ServerLabel;
use linkerd_app_core::{
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Result,
};
use linkerd_cache::Cached;
pub use linkerd_server_policy::{
    authz::Suffix, Authentication, Authorization, Meta, Protocol, ServerPolicy,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;

#[derive(Clone, Debug, Error)]
#[error("unauthorized connection on {}/{}", server.kind, server.name)]
pub struct DeniedUnauthorized {
    server: Arc<Meta>,
}

pub trait GetPolicy {
    // Returns the traffic policy configured for the destination address.
    fn get_policy(&self, dst: OrigDstAddr) -> AllowPolicy;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(ServerPolicy),
    Deny,
}

#[derive(Clone, Debug)]
pub struct AllowPolicy {
    dst: OrigDstAddr,
    server: Cached<watch::Receiver<ServerPolicy>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerPermit {
    pub dst: OrigDstAddr,
    pub protocol: Protocol,

    pub labels: ServerAuthzLabels,
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p)
    }
}

impl From<DefaultPolicy> for ServerPolicy {
    fn from(d: DefaultPolicy) -> Self {
        match d {
            DefaultPolicy::Allow(p) => p,
            DefaultPolicy::Deny => ServerPolicy {
                protocol: Protocol::Opaque,
                authorizations: vec![],
                meta: Arc::new(Meta {
                    group: "default".into(),
                    kind: "default".into(),
                    name: "deny".into(),
                }),
            },
        }
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
        let server = Cached::uncached(server);
        let p = Self { dst, server };
        (p, tx)
    }

    #[inline]
    pub(crate) fn protocol(&self) -> Protocol {
        self.server.borrow().protocol
    }

    #[inline]
    pub fn dst_addr(&self) -> OrigDstAddr {
        self.dst
    }

    #[inline]
    pub fn group(&self) -> String {
        self.server.borrow().meta.group.to_string()
    }

    #[inline]
    pub fn kind(&self) -> String {
        self.server.borrow().meta.kind.to_string()
    }

    #[inline]
    pub fn name(&self) -> String {
        self.server.borrow().meta.name.to_string()
    }

    #[inline]
    pub fn server_label(&self) -> ServerLabel {
        ServerLabel(self.server.borrow().meta.clone())
    }

    async fn changed(&mut self) {
        if self.server.changed().await.is_err() {
            // If the sender was dropped, then there can be no further changes.
            futures::future::pending::<()>().await;
        }
    }

    /// Checks whether the server has any authorizations at all. If it does not,
    /// a denial error is returned.
    pub(crate) fn check_port_allowed(self) -> Result<Self, DeniedUnauthorized> {
        let server = self.server.borrow();

        if server.authorizations.is_empty() {
            return Err(DeniedUnauthorized {
                server: server.meta.clone(),
            });
        }
        drop(server);

        Ok(self)
    }

    /// Checks whether the destination port's `AllowPolicy` is authorized to
    /// accept connections given the provided TLS state.
    pub(crate) fn check_authorized(
        &self,
        client_addr: Remote<ClientAddr>,
        tls: &tls::ConditionalServerTls,
    ) -> Result<ServerPermit, DeniedUnauthorized> {
        let server = self.server.borrow();
        for authz in server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&client_addr.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => {
                        return Ok(ServerPermit::new(self.dst, &*server, authz));
                    }

                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(ServerPermit::new(self.dst, &*server, authz));
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
                            if identities.contains(id.as_str())
                                || suffixes.iter().any(|s| s.contains(id.as_str()))
                            {
                                return Ok(ServerPermit::new(self.dst, &*server, authz));
                            }
                        }
                    }
                }
            }
        }

        Err(DeniedUnauthorized {
            server: server.meta.clone(),
        })
    }
}

// === impl ServerPermit ===

impl ServerPermit {
    fn new(dst: OrigDstAddr, server: &ServerPolicy, authz: &Authorization) -> Self {
        Self {
            dst,
            protocol: server.protocol,
            labels: ServerAuthzLabels {
                authz: authz.meta.clone(),
                server: ServerLabel(server.meta.clone()),
            },
        }
    }
}
