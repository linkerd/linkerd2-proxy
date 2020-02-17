use self::waiter::Notify;
pub(crate) use self::waiter::Wait;
use crate::error::Error;
use futures::{Async, Poll};
use std::sync::Arc;
use tracing::trace;

/// The shared state between one or more Lock instances.
///
/// When multiple lock instances are contending for the inner value, waiters are stored in a LIFO
/// stack. This is done to bias for latency instead of fairness
///
/// N.B. Waiters capacity is released lazily as waiters are notified and, i.e., not when a waiter is
/// dropped. In high-load scenarios where the lock _always_ has new waiters, this could potentially
/// manifest as unbounded memory growth. This situation is not expected to arise in normal operation.
pub(crate) struct Shared<T> {
    state: State<T>,
    waiters: Vec<Notify>,
}

enum State<T> {
    /// A Lock is holding the value.
    Claimed,

    /// The inner value is available.
    Unclaimed(T),

    /// The lock has failed.
    Failed(Arc<Error>),
}

// === impl Shared ===

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self {
            waiters: Vec::new(),
            state: State::Unclaimed(value),
        }
    }

    /// Try to claim a value without registering a waiter.
    ///
    /// Once a value is acquired it **must** be returned via `release_and_notify`.
    pub fn try_acquire(&mut self) -> Result<Option<T>, Arc<Error>> {
        match std::mem::replace(&mut self.state, State::Claimed) {
            // This lock has acquired the value.
            State::Unclaimed(v) => {
                trace!("acquired");
                Ok(Some(v))
            }
            // The value is already claimed by a lock.
            State::Claimed => Ok(None),
            // The lock has failed, so reset the state immediately so that all instances may be
            // notified.
            State::Failed(error) => {
                self.state = State::Failed(error.clone());
                Err(error)
            }
        }
    }

    /// Try to acquire a value or register the given waiter to be notified when
    /// the lock is available.
    ///
    /// Once a value is acquired it **must** be returned via `release_and_notify`.
    ///
    /// If `Async::NotReady` is returned, the polling task, once notified, **must** either call
    /// `poll_acquire` again to obtain a value, or the waiter **must** be returned via
    /// `release_waiter`.
    pub fn poll_acquire(&mut self, wait: &Wait) -> Poll<T, Arc<Error>> {
        match self.try_acquire() {
            Ok(Some(svc)) => Ok(Async::Ready(svc)),
            Ok(None) => {
                // Register the current task to be notified.
                wait.register();
                // Register the waiter's notify handle if one isn't already registered.
                if let Some(notify) = wait.get_notify() {
                    trace!("Registering waiter");
                    self.waiters.push(notify);
                }
                debug_assert!(wait.has_notify());
                Ok(Async::NotReady)
            }
            Err(error) => Err(error),
        }
    }

    pub fn release_and_notify(&mut self, value: T) {
        trace!(waiters = self.waiters.len(), "Releasing");
        assert!(match self.state {
            State::Claimed => true,
            _ => false,
        });
        self.state = State::Unclaimed(value);
        self.notify_next_waiter();
    }

    pub fn release_waiter(&mut self, wait: Wait) {
        // If a waiter is being released and it does not have a notify, then it must be being
        // released after being notified. Notify the next waiter to prevent deadlock.
        if let State::Unclaimed(_) = self.state {
            if !wait.has_notify() {
                self.notify_next_waiter();
            }
        }
    }

    fn notify_next_waiter(&mut self) {
        while let Some(waiter) = self.waiters.pop() {
            if waiter.notify() {
                trace!("Notified waiter");
                return;
            }
        }
    }

    pub fn fail(&mut self, error: Arc<Error>) {
        trace!(waiters = self.waiters.len(), %error, "Failing");
        assert!(match self.state {
            State::Claimed => true,
            _ => false,
        });
        self.state = State::Failed(error);

        while let Some(waiter) = self.waiters.pop() {
            waiter.notify();
        }
    }
}

mod waiter {
    use futures::task::AtomicTask;
    use std::sync::{Arc, Weak};

    /// A handle held by Lock instances when waiting to be notified.
    #[derive(Default)]
    pub(crate) struct Wait(Arc<AtomicTask>);

    /// A handle held by shared lock state to notify a waiting Lock.
    ///
    /// There may be at most one `Notify` instance per `Wait` instance at any one time.
    pub(super) struct Notify(Weak<AtomicTask>);

    impl Wait {
        /// If a `Notify` handle does not currently exist for this waiter, create a new one.
        pub(super) fn get_notify(&self) -> Option<Notify> {
            if !self.has_notify() {
                let n = Notify(Arc::downgrade(&self.0));
                debug_assert!(self.has_notify());
                Some(n)
            } else {
                None
            }
        }

        /// Register this waiter with the current task.
        pub(super) fn register(&self) {
            self.0.register();
        }

        /// Returns true iff there is currently a `Notify` handle for this waiter.
        pub(super) fn has_notify(&self) -> bool {
            let weaks = Arc::weak_count(&self.0);
            debug_assert!(
                weaks == 0 || weaks == 1,
                "There must only be at most one Notify per Wait"
            );
            weaks == 1
        }
    }

    impl std::fmt::Debug for Wait {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Wait(notify={})", self.has_notify())
        }
    }

    impl Notify {
        /// Attempt to notify the waiter.
        ///
        /// Returns true if a waiter was notified and false otherwise.
        pub fn notify(self) -> bool {
            if let Some(task) = self.0.upgrade() {
                task.notify();
                true
            } else {
                false
            }
        }
    }
}
