use indexmap::IndexSet;
use linkerd2_app_core::{admit, transport::tls};
use std::sync::Arc;

/// A connection policy that drops
#[derive(Clone, Debug)]
pub struct RequireIdentityForPorts {
    ports: Arc<IndexSet<u16>>,
}

#[derive(Debug)]
pub struct IdentityRequired(());

impl<T: IntoIterator<Item = u16>> From<T> for RequireIdentityForPorts {
    fn from(ports: T) -> Self {
        Self {
            ports: Arc::new(ports.into_iter().collect()),
        }
    }
}

impl admit::Admit<tls::accept::Connection> for RequireIdentityForPorts {
    type Error = IdentityRequired;

    fn admit(&mut self, (meta, _): &tls::accept::Connection) -> Result<(), Self::Error> {
        let port = meta.addrs.target_addr().port();
        let id_required = self.ports.contains(&port);

        tracing::debug!(%port, peer.id = ?meta.peer_identity, %id_required);
        if id_required && meta.peer_identity.is_none() {
            return Err(IdentityRequired(()));
        }

        Ok(())
    }
}

impl std::fmt::Display for IdentityRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "identity required")
    }
}

impl std::error::Error for IdentityRequired {}
