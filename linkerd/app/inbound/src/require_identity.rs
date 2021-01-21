use crate::target::TcpAccept;
use indexmap::IndexSet;
use linkerd_app_core::{svc::stack::Predicate, Conditional, Error};
use std::sync::Arc;

/// A connection policy that fails direct connections that don't have a client
/// identity.
#[derive(Clone, Debug)]
pub struct RequireIdentityForDirect;

/// A connection policy that fails connections that don't have a client identity
/// if they target one of the configured local ports.
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
            Conditional::Some(Some(_)) => Ok(meta),
            _ => Err(IdentityRequired(()).into()),
        }
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
                Conditional::Some(Some(_)) => Ok(meta),
                _ => Err(IdentityRequired(()).into()),
            }
        } else {
            Ok(meta)
        }
    }
}

// === impl IdentityRequired ===

impl std::fmt::Display for IdentityRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "identity required")
    }
}

impl std::error::Error for IdentityRequired {}
