use crate::target::TcpAccept;
use indexmap::IndexSet;
use linkerd_app_core::{svc::stack::Predicate, tls, Conditional, Error};
use std::sync::Arc;

/// A connection policy that fails connections that don't have a client identity
/// if they target one of the configured local ports.
#[derive(Clone, Debug)]
pub struct RequireIdentityForPorts {
    ports: Arc<IndexSet<u16>>,
}

#[derive(Debug)]
pub struct IdentityRequired(());

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

        tracing::debug!(%port, tls = ?meta.tls, %id_required);
        if id_required {
            match meta.tls {
                Conditional::Some(tls::ServerTls::Established {
                    client_id: Some(_), ..
                }) => Ok(meta),
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
