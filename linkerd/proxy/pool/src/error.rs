//! Error types for the `PoolQueue` middleware.

use linkerd_error::Error;
use std::{fmt, sync::Arc};

/// A shareable, terminal error produced by either a service or discovery
/// resolution.
#[derive(Clone, Debug)]
pub struct TerminalFailure {
    inner: Arc<Error>,
}

/// An error produced when the a buffer's worker closes unexpectedly.
#[derive(Debug, thiserror::Error)]
#[error("buffer worker closed unexpectedly")]
pub struct Closed(());

// === impl TerminalFailure ===

impl TerminalFailure {
    pub(crate) fn new(inner: Error) -> TerminalFailure {
        let inner = Arc::new(inner);
        TerminalFailure { inner }
    }
}

impl fmt::Display for TerminalFailure {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "buffered service failed: {}", self.inner)
    }
}

impl std::error::Error for TerminalFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&**self.inner)
    }
}

// === impl Closed ====

impl Closed {
    pub(crate) fn new() -> Self {
        Closed(())
    }
}
