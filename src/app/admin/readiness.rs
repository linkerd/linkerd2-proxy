use std::sync::{Arc, Weak};

/// Tracks the processes's readiness to serve traffic.
///
/// Once `is_ready()` returns true, it will never return false.
#[derive(Clone, Debug)]
pub struct Readiness(Weak<()>);

/// When all latches are dropped, the process is considered ready.
#[derive(Clone, Debug)]
pub struct Latch(Arc<()>);

impl Readiness {
    pub fn new() -> (Readiness, Latch) {
        let r = Arc::new(());
        (Readiness(Arc::downgrade(&r)), Latch(r))
    }

    pub fn is_ready(&self) -> bool {
        self.0.upgrade().is_none()
    }
}

/// ALways ready.
impl Default for Readiness {
    fn default() -> Self {
        Self::new().0
    }
}

impl Latch {
    /// Releases this readiness latch.
    pub fn release(self) {
        drop(self);
    }
}
