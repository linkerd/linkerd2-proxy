use crate::target::TcpAccept;
use indexmap::IndexSet;
use linkerd_app_core::{svc::stack::Predicate, Conditional, Error};
use std::sync::Arc;

/// A connection policy that drops
#[derive(Clone, Debug)]
pub struct RequireIdentityForDirect;

/// A connection policy that drops
#[derive(Clone, Debug)]
pub struct RequireIdentityForPorts {
    ports: Arc<IndexSet<u16>>,
}

#[derive(Debug)]
pub struct IdentityRequired(());

// === impl RequireIdentityForDirect ===

impl Predicate<TcpAccept> for RequireIdentityForDirect {
    type Request = TcpAccept;

    fn check(&mut self, meta: TcpAccept) -> Result<TcpAccept, Error> {
        tracing::debug!(client.id = ?meta.client_id);
        match meta.client_id {
            Conditional::Some(Some(_)) => {}
            _ => return Err(IdentityRequired(()).into()),
        }
        Ok(meta)
    }
}

// === impl RequireIdentityForPorts ===

impl<T: IntoIterator<Item = u16>> From<T> for RequireIdentityForPorts {
    fn from(ports: T) -> Self {
        Self {
            ports: Arc::new(ports.into_iter().collect()),
        }
    }
}

impl Predicate<TcpAccept> for RequireIdentityForPorts {
    type Request = TcpAccept;

    fn check(&mut self, meta: TcpAccept) -> Result<TcpAccept, Error> {
        let port = meta.target_addr.port();
        let id_required = self.ports.contains(&port);

        tracing::debug!(%port, client.id = ?meta.client_id, %id_required);
        if id_required {
            match meta.client_id {
                Conditional::Some(Some(_)) => {}
                _ => return Err(IdentityRequired(()).into()),
            }
        }

        Ok(meta)
    }
}

// === impl IdentityRequired ===

impl std::fmt::Display for IdentityRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "identity required")
    }
}

impl std::error::Error for IdentityRequired {}
