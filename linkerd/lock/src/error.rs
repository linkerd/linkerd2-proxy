pub use linkerd2_error::Error;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ServiceError(Arc<Error>);

// === impl ServiceError ===

impl ServiceError {
    pub(crate) fn new(e: Arc<Error>) -> Self {
        ServiceError(e)
    }

    pub fn inner(&self) -> &Error {
        self.0.as_ref()
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ServiceError {}
