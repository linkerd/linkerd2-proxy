use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
pub use tokio::sync::OwnedMutexGuard as Guard;

/// Provides mutually exclusive to a `T`-typed value, asynchronously.
pub struct Lock<T> {
    /// Set when this Lock is interested in acquiring the value.
    waiting: Option<Pin<Box<dyn Future<Output = Guard<T>> + Send + 'static>>>,
    lock: Arc<Mutex<T>>,
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(value: S) -> Self {
        Self {
            waiting: None,
            lock: Arc::new(Mutex::new(value)),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            waiting: None,
            lock: self.lock.clone(),
        }
    }
}

impl<T: Send + 'static> Lock<T> {
    pub fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<Guard<T>> {
        loop {
            self.waiting = match self.waiting {
                // This instance has not registered interest in the lock.
                None => Some(Box::pin(self.lock.clone().lock_owned())),

                // This instance is interested in the lock.
                Some(ref mut waiter) => {
                    tokio::pin!(waiter);
                    return waiter.poll(cx);
                }
            };
        }
    }
}

impl<T> Lock<T> {
    // optimization so we can elide the `Box::pin` when we just want a future
    pub async fn acquire(&mut self) -> Guard<T> {
        // Are we already waiting? If so, reuse that...
        if let Some(waiting) = self.waiting.take() {
            waiting.await
        } else {
            self.lock.clone().lock_owned().await
        }
    }
}
