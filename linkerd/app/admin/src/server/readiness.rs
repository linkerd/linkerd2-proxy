use std::sync::{atomic::AtomicBool, Arc};

#[derive(Clone, Debug)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    pub fn new(init: bool) -> Readiness {
        Readiness(Arc::new(init.into()))
    }

    pub fn get(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set(&self, ready: bool) {
        self.0.store(ready, std::sync::atomic::Ordering::Release)
    }
}
