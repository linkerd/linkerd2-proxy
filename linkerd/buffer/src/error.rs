use linkerd2_error::Error;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct Closed(pub(crate) ());

#[derive(Clone, Debug)]
pub struct ServiceError(pub(crate) Arc<Error>);

// === impl Closed ===

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Closed")
    }
}

impl std::error::Error for Closed {}

// === impl ServiceError ===

impl ServiceError {
    pub fn inner(&self) -> &Error {
        self.0.as_ref()
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.as_ref().fmt(f)
    }
}

impl std::error::Error for ServiceError {}
