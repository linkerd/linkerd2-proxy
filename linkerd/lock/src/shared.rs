use self::waiter::Notify;
pub(crate) use self::waiter::Wait;
use futures::Async;
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
    /// Set when the value is available to be acquired; None when the value is acquired.
    value: Option<T>,

    /// A LIFO stack of waiters to be notified when the value is available.
    waiters: Vec<Notify>,
}

// === impl Shared ===

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self {
            waiters: Vec::new(),
            value: Some(value),
        }
    }

    /// Try to claim a value without registering a waiter.
    ///
    /// Once a value is acquired it **must** be returned via `release_and_notify`.
    pub fn acquire(&mut self) -> Option<T> {
        trace!(acquired = %self.value.is_some());
        self.value.take()
    }

    /// Try to acquire a value or register the given waiter to be notified when
    /// the lock is available.
    ///
    /// Once a value is acquired it **must** be returned via `release_and_notify`.
    ///
    /// If `Async::NotReady` is returned, the polling task, once notified, **must** either call
    /// `poll_acquire` again to obtain a value, or the waiter **must** be returned via
    /// `release_waiter`.
    pub fn poll_acquire(&mut self, wait: &Wait) -> Async<T> {
        if let Some(value) = self.acquire() {
            return Async::Ready(value);
        }

        // Register the current task to be notified.
        wait.register();
        // Register the waiter's notify handle if one isn't already registered.
        if let Some(notify) = wait.get_notify() {
            self.waiters.push(notify);
        }
        debug_assert!(wait.has_notify());

        trace!(waiters = self.waiters.len(), "Waiting");
        Async::NotReady
    }

    pub fn release_and_notify(&mut self, value: T) {
        trace!(waiters = self.waiters.len(), "Releasing");
        assert!(self.value.is_none());
        self.value = Some(value);
        self.notify_next_waiter();
    }

    pub fn release_waiter(&mut self, wait: Wait) {
        // If a waiter is being released and it does not have a notify, then it must be being
        // released after being notified. Notify the next waiter to prevent deadlock.
        if self.value.is_some() {
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
