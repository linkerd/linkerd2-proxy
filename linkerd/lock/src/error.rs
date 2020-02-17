pub use linkerd2_error::Error;
use std::sync::Arc;

#[derive(Debug)]
pub struct Poisoned(());

#[derive(Debug)]
pub struct ServiceError(Arc<Error>);

// === impl POisoned ===

impl Poisoned {
    pub fn new() -> Self {
        Poisoned(())
    }
}

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "poisoned")
    }
}

impl std::error::Error for Poisoned {}

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
