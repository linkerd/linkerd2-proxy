use std::sync::{Arc, Weak};

#[derive(Clone, Debug)]
pub struct Readiness(Weak<()>);

#[derive(Clone, Debug)]
pub struct Latch(Arc<()>);

impl Readiness {
    pub fn new() -> (Readiness, Latch) {
        let r = Arc::new(());
        (Readiness(Arc::downgrade(&r)), Latch(r))
    }

    pub fn ready(&self) -> bool {
        self.0.upgrade().is_none()
    }
}

impl Default for Readiness {
    fn default() -> Self {
        // Is immediately ready.
        Self::new().0
    }
}
