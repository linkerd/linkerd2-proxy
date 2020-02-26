//! Error types

use std::fmt;
use std::time::Duration;

pub use tokio_timer::Error as TimerError;

/// An error representing that an operation timed out.
#[derive(Debug)]
pub struct ResponseTimeout(pub(crate) Duration);

/// A duration which pretty-prints as fractional seconds.
#[derive(Copy, Clone, Debug)]
struct HumanDuration<'a>(&'a Duration);

// === impl ResponseTimeout ===

impl ResponseTimeout {
    /// Get the amount of time waited until this error was triggered.
    pub fn duration(&self) -> Duration {
        self.0
    }
}

impl fmt::Display for ResponseTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "operation timed out after {}", HumanDuration(&self.0))
    }
}

impl std::error::Error for ResponseTimeout {}

impl<'a> fmt::Display for HumanDuration<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "{}ms", subsec_ms)
        } else {
            write!(fmt, "{}s", secs as f64 + subsec_ms)
        }
    }
}
