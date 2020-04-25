use std::task::{Context, Poll};
use std::{cell::UnsafeCell, future::Future, pin::Pin, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Provides mutually exclusive to a `T`-typed value, asynchronously.
pub struct Lock<T> {
    /// Set when this Lock is interested in acquiring the value.
    waiting: Option<Pin<Box<dyn Future<Output = OwnedSemaphorePermit> + 'static>>>,
    sem: Arc<Semaphore>,
    value: Arc<UnsafeCell<T>>,
}

/// Guards access to a `T`-typed value, ensuring the value is released on Drop.
pub struct Guard<T> {
    value: Arc<UnsafeCell<T>>,
    // Hang onto this to drop it when the access ends.
    _permit: OwnedSemaphorePermit,
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(value: S) -> Self {
        Self {
            waiting: None,
            // XXX: Bummer that these have to be arced separately and we can't
            // them in a single `Arc`, but `Semaphore::acquire_owned` needs an
            // `Arc<Self>` receiver...
            sem: Arc::new(Semaphore::new(1)),
            value: Arc::new(UnsafeCell::new(value)),
        }
    }
}

impl<S> Clone for Lock<S> {
    fn clone(&self) -> Self {
        Self {
            // Clones have an independent local lock state.
            waiting: None,
            sem: self.sem.clone(),
            value: self.value.clone(),
        }
    }
}

impl<T> Lock<T> {
    pub fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<Guard<T>> {
        loop {
            self.waiting = match self.waiting {
                // This instance has not registered interest in the lock.
                None => Some(Box::pin(self.sem.clone().acquire_owned())),

                // This instance is interested in the lock.
                Some(ref mut waiter) => {
                    tokio::pin!(waiter);
                    let _permit = futures::ready!(waiter.poll(cx));
                    self.waiting = None;
                    return Poll::Ready(Guard {
                        value: self.value.clone(),
                        _permit,
                    });
                }
            };
        }
    }
}

impl<T> std::ops::Deref for Guard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // Safety: creating a `Guard` means that the single permit in the
        // semaphore has been acquired and we have exclusive access to the
        // value.
        unsafe { &*self.value.get() }
    }
}

impl<T> std::ops::DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: creating a `Guard` means that the single permit in the
        // semaphore has been acquired and we have exclusive access to the
        // value.
        unsafe { &mut *self.value.get() }
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

// Safety: As long as T: Send, it's fine to send and share Lock<T> between threads.
// If T was not Send, sending and sharing a Lock<T> would be bad, since you can access T through
// Lock<T>.
unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}
unsafe impl<T: Send> Send for Guard<T> {}
unsafe impl<T: Send + Sync> Sync for Guard<T> {}
