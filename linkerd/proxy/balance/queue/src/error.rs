//! Error types for the `PoolQueue` middleware.

use linkerd_error::Error;
use std::{fmt, sync::Arc};

/// A shareable, terminal error produced by either a service or discovery
/// resolution.
#[derive(Clone, Debug)]
pub struct TerminalFailure(Arc<Error>);

// === impl TerminalFailure ===

impl TerminalFailure {
    pub(crate) fn new(inner: Error) -> TerminalFailure {
        TerminalFailure(Arc::new(inner))
    }
}

impl fmt::Display for TerminalFailure {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "pool failed: {}", self.0)
    }
}

impl std::error::Error for TerminalFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&**self.0)
    }
}
