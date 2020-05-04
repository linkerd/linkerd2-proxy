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
    /// Attempt to acquire the lock, returning `Pending` if it is held
    /// elsewhere.
    ///
    /// If this `Lock` instance is not
    pub fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<Guard<T>> {
        // If we have already registered interest in the lock and are waiting on
        // a future, we'll poll that. Otherwise, we need a future.
        //
        // We `take` the waiting future so that if we drive it to completion on
        // this poll, it won't be set on subsequent polls.
        let mut waiting = self.waiting.take().unwrap_or_else(|| {
            // This instance has not registered interest in the lock.
            Box::pin(self.lock.clone().lock_owned())
        });

        // Poll the future.
        let res = {
            let future = &mut waiting;
            tokio::pin!(future);
            future.poll(cx)
        };

        tracing::trace!(ready = res.is_ready());

        // If the future hasn't completed, save it to be polled again the next
        // time `poll_acquire` is called.
        if res.is_pending() {
            self.waiting = Some(waiting);
        }

        res
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
