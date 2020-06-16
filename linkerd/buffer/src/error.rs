use linkerd2_error::Error;
use std::sync::Arc;
use std::time::Duration;

#[derive(Copy, Clone, Debug)]
pub struct Closed(pub(crate) ());

#[derive(Clone, Debug)]
pub struct ServiceError(pub(crate) Arc<Error>);

#[derive(Copy, Clone, Debug)]
pub struct IdleError(pub(crate) Duration);

// === impl Closed ===

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "closed")
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

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&**self.0.as_ref())
    }
}

// === impl IdleError ===

impl<'a> std::fmt::Display for IdleError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "Service idled out after {}ms", subsec_ms)
        } else {
            write!(
                fmt,
                "Service idled out after {}s",
                secs as f64 + subsec_ms / 1000.0,
            )
        }
    }
}

impl std::error::Error for IdleError {}
