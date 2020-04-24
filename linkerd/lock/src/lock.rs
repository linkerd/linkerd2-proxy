use std::task::{Context, Poll};
use std::{cell::UnsafeCell, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Semaphore;

/// Provides mutually exclusive to a `T`-typed value, asynchronously.
pub struct Lock<T> {
    /// Set when this Lock is interested in acquiring the value.
    waiting: Option<Pin<Box<dyn Future<Output = Guard<T>> + 'static>>>,
    shared: Arc<Shared<T>>,
}

/// Guards access to a `T`-typed value, ensuring the value is released on Drop.
pub struct Guard<T> {
    shared: Arc<Shared<T>>,
}

struct Shared<T: ?Sized> {
    sem: Semaphore,
    value: UnsafeCell<T>,
}

// === impl Lock ===

impl<S> Lock<S> {
    pub fn new(value: S) -> Self {
        Self {
            waiting: None,
            shared: Arc::new(Shared {
                sem: Semaphore::new(1),
                value: UnsafeCell::new(value),
            }),
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

impl<T: 'static> Lock<T> {
    pub fn poll_acquire(&mut self, cx: &mut Context<'_>) -> Poll<Guard<T>> {
        loop {
            self.waiting = match self.waiting {
                // This instance has not registered interest in the lock.
                None => {
                    let shared = self.shared.clone();
                    Some(Box::pin(async move {
                        let permit = shared.sem.acquire().await;
                        // We cannot use the `tokio::sync::semaphore::Permit`
                        // type for the release-on-drop behavior, because it borrows
                        // the semaphore. Instead, we will manually release the
                        // permit when dropping a guard.
                        permit.forget();
                        Guard { shared }
                    }))
                }

                // This instance is interested in the lock.
                Some(ref mut waiter) => {
                    tokio::pin!(waiter);
                    let guard = futures::ready!(waiter.poll(cx));
                    self.waiting = None;
                    return Poll::Ready(guard);
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
        unsafe { &*self.shared.value.get() }
    }
}

impl<T> std::ops::DerefMut for Guard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: creating a `Guard` means that the single permit in the
        // semaphore has been acquired and we have exclusive access to the
        // value.
        unsafe { &mut *self.shared.value.get() }
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

impl<T> Drop for Lock<T> {
    fn drop(&mut self) {
        // Release the single permit back to the semaphore.
        self.shared.sem.add_permits(0);
    }
}

impl<T> Drop for Guard<T> {
    fn drop(&mut self) {
        // Release the single permit back to the semaphore.
        self.shared.sem.add_permits(1);
    }
}

// Safety: As long as T: Send, it's fine to send and share Lock<T> between threads.
// If T was not Send, sending and sharing a Lock<T> would be bad, since you can access T through
// Lock<T>.
unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}
unsafe impl<T: Send> Send for Guard<T> {}
unsafe impl<T: Send + Sync> Sync for Guard<T> {}
