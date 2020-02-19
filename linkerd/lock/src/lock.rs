use crate::shared::{Shared, Wait};
use futures::Async;
use std::sync::{Arc, Mutex};

/// A middleware that safely shares an inner service among clones.
///
/// As the service is polled to readiness, the lock is acquired and the inner service is polled. If
/// the service is cloned, the service's lock state isnot retained by the clone.
pub struct Lock<T> {
    /// Set when this Lock is interested in acquiring the value.
    waiting: Option<Wait>,
    shared: Arc<Mutex<Shared<T>>>,
}

/// Guards access to a `T`-typed value, ensuring the value is released on Drop.
pub struct Guard<T> {
    /// Must always be Some; Used to reclaim the value in Drop.
    value: Option<T>,

    shared: Arc<Mutex<Shared<T>>>,
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(service: S) -> Self {
        Self {
            waiting: None,
            shared: Arc::new(Mutex::new(Shared::new(service))),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            waiting: None,
            shared: self.shared.clone(),
        }
    }
}

impl<T> Lock<T> {
    fn guard(&self, value: T) -> Guard<T> {
        Guard {
            value: Some(value),
            shared: self.shared.clone(),
        }
    }

    pub fn poll_acquire(&mut self) -> Async<Guard<T>> {
        let mut shared = self.shared.lock().expect("Lock poisoned");

        loop {
            self.waiting = match self.waiting {
                // This instance has not registered interest in the lock.
                None => match shared.acquire() {
                    // This instance has exclusive access to the inner service.
                    Some(value) => {
                        // The state remains Released.
                        return Async::Ready(self.guard(value));
                    }
                    None => Some(Wait::default()),
                },

                // This instance is interested in the lock.
                Some(ref waiter) => match shared.poll_acquire(waiter) {
                    Async::NotReady => return Async::NotReady,
                    Async::Ready(value) => {
                        self.waiting = None;
                        return Async::Ready(self.guard(value));
                    }
                },
            };
        }
    }
}

impl<T> Drop for Lock<T> {
    fn drop(&mut self) {
        if let Some(wait) = self.waiting.take() {
            if let Ok(mut shared) = self.shared.lock() {
                shared.release_waiter(wait);
            }
        }
    }
}

impl<T> std::ops::Deref for Guard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value.as_ref().expect("Value dropped from guard")
    }
}

impl<T> std::ops::DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().expect("Value dropped from guard")
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Guard<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Guard({:?})", &**self)
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Guard<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        let value = self.value.take().expect("Guard may not be dropped twice");
        if let Ok(mut shared) = self.shared.lock() {
            shared.release_and_notify(value);
        }
    }
}
