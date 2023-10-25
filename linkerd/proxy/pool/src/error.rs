//! Error types for the `Buffer` middleware.

use linkerd_error::Error;
use std::{fmt, sync::Arc};

/// A shareable, terminal error produced by either a service or discovery
/// resolution.
///
/// [`Service`]: crate::Service
/// [`Buffer`]: crate::buffer::Buffer
#[derive(Clone, Debug)]
pub struct TerminalFailure {
    inner: Arc<Error>,
}

/// An error produced when the a buffer's worker closes unexpectedly.
pub struct Closed {
    _p: (),
}

// ===== impl ServiceError =====

impl TerminalFailure {
    pub(crate) fn new(inner: Error) -> TerminalFailure {
        let inner = Arc::new(inner);
        TerminalFailure { inner }
    }

    // Private to avoid exposing `Clone` trait as part of the public API
    pub(crate) fn clone(&self) -> TerminalFailure {
        TerminalFailure {
            inner: self.inner.clone(),
        }
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

// ===== impl Closed =====

impl Closed {
    pub(crate) fn new() -> Self {
        Closed { _p: () }
    }
}

impl fmt::Debug for Closed {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_tuple("Closed").finish()
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("buffer's worker closed unexpectedly")
    }
}

impl std::error::Error for Closed {}
