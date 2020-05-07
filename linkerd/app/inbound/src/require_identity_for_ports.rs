use indexmap::IndexSet;
use linkerd2_app_core::transport::admit;
use linkerd2_app_core::transport::tls;
use linkerd2_app_core::Error;
use std::sync::Arc;

#[derive(Clone)]
pub struct RequireIdentityForPorts {
    ports: Arc<IndexSet<u16>>,
}

#[derive(Debug)]
pub struct IdentityRequired;

impl RequireIdentityForPorts {
    pub fn new(ports: Arc<IndexSet<u16>>) -> Self {
        Self { ports }
    }
}

impl admit::Admit<tls::accept::Connection> for RequireIdentityForPorts {
    type Error = Error;

    fn admit(&mut self, (meta, _): &tls::accept::Connection) -> Result<(), Self::Error> {
        let port = meta.addrs.target_addr().port();
        if self.ports.contains(&port) && meta.peer_identity.is_none() {
            Err(IdentityRequired.into())
        } else {
            Ok(())
        }
    }
}

impl std::fmt::Display for IdentityRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "identity required")
    }
}
impl std::error::Error for IdentityRequired {}
