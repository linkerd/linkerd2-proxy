//! Error types

use std::fmt;
use std::time::Duration;

pub use timer::Error as Timer;

pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;

/// An error representing that an operation timed out.
#[derive(Debug)]
pub struct Timedout(pub(crate) Duration);

/// A duration which pretty-prints as fractional seconds.
#[derive(Copy, Clone, Debug)]
struct HumanDuration<'a>(&'a Duration);

//===== impl Timedout =====

impl Timedout {
    /// Get the amount of time waited until this error was triggered.
    pub fn duration(&self) -> Duration {
        self.0
    }
}

impl fmt::Display for Timedout {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "operation timed out after {}", HumanDuration(&self.0))
    }
}

impl std::error::Error for Timedout {}

impl<'a> fmt::Display for HumanDuration<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "{}ms", subsec_ms)
        } else {
            write!(fmt, "{}s", secs as f64 + subsec_ms)
        }
    }
}
