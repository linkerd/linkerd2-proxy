use crate::Service;
use futures::{ready, FutureExt};
use std::{
    sync::{atomic::AtomicBool, Arc},
    task::{Context, Poll},
};
use tokio::sync::Notify;
use tokio_util::sync::ReusableBoxFuture;
use tracing::{debug, trace};

/// A middleware that alters its readiness state according to a gate channel.
pub struct Gate<S> {
    inner: S,
    rx: Rx,
    is_waiting: bool,
    waiting: ReusableBoxFuture<'static, ()>,
}

/// Observes gate state changes.
#[derive(Clone, Debug)]
pub struct Rx(Arc<Shared>);

/// Changes the gate state.
#[derive(Clone, Debug)]
pub struct Tx(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    open: AtomicBool,
    notify: Notify,
}

/// Creates a new gate channel.
pub fn channel() -> (Tx, Rx) {
    let shared = Arc::new(Shared {
        open: AtomicBool::new(true),
        notify: Notify::new(),
    });
    (Tx(shared.clone()), Rx(shared))
}

// === impl Rx ===

impl Rx {
    /// Indicates whether the gate is open.
    pub fn is_open(&self) -> bool {
        self.0.open.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Indicates whether the gate is closed.
    #[inline]
    pub fn is_shut(&self) -> bool {
        !self.is_open()
    }

    /// Waits for the gate state to change.
    pub async fn changed(&self) -> bool {
        self.0.notify.notified().await;
        self.is_open()
    }
}

// === impl Tx ===

impl Tx {
    /// Opens the gate.
    pub fn open(&self) {
        if !self.0.open.swap(true, std::sync::atomic::Ordering::Release) {
            debug!("Gate opened");
            self.0.notify.notify_waiters();
        }
    }

    /// Closes the gate.
    pub fn shut(&self) {
        if self
            .0
            .open
            .swap(false, std::sync::atomic::Ordering::Release)
        {
            debug!("Gate shut");
            self.0.notify.notify_waiters();
        }
    }
}

// === impl Gate ===

impl<S> Gate<S> {
    pub fn channel(inner: S) -> (Tx, Self) {
        let (tx, rx) = channel();
        (tx, Self::new(inner, rx))
    }

    pub fn new(inner: S, rx: Rx) -> Self {
        let (waiting, is_waiting) = if rx.is_open() {
            let waiting = ReusableBoxFuture::new(async { unreachable!() });
            (waiting, false)
        } else {
            let Rx(rx) = rx.clone();
            let waiting = ReusableBoxFuture::new(async move {
                rx.notify.notified().await;
            });
            (waiting, true)
        };

        Self {
            inner,
            rx,
            is_waiting,
            waiting,
        }
    }
}

impl<S> Clone for Gate<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.inner.clone(), self.rx.clone())
    }
}

impl<Req, S> Service<Req> for Gate<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If the gate is shut, wait for it to open by storing a future that
        // will complete when it opens. This ensures that waiters are notified.
        while self.rx.is_shut() {
            trace!(gate.open = false);
            if !self.is_waiting {
                let rx = self.rx.clone();
                self.waiting.set(async move {
                    rx.changed().await;
                });
                self.is_waiting = true;
            }
            ready!(self.waiting.poll_unpin(cx));
        }

        debug_assert!(self.rx.is_open());
        self.is_waiting = false;
        trace!(gate.open = true);

        // When the gate is open, poll the inner service.
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceExt;

    #[tokio::test]
    async fn gate() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(inner, rx.clone()));

        handle.allow(0);
        assert!(gate.poll_ready().is_pending());

        tx.shut();
        assert!(gate.poll_ready().is_pending());

        tx.open();
        assert!(gate.poll_ready().is_pending());

        handle.allow(1);
        assert!(gate.poll_ready().is_ready());
    }

    #[tokio::test]
    async fn notifies_on_open() {
        let (tx, rx) = channel();
        let (inner, mut handle) = tower_test::mock::pair::<(), ()>();
        let mut gate = Gate::new(inner, rx);

        // Start with a shut gate on an available inner service.
        handle.allow(1);
        tx.shut();

        // Wait for the gated service to become ready.
        let mut ready = gate.ready();
        tokio::select! {
            biased;
            _ = &mut ready => panic!("unexpected ready"),
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
        }

        // Open the gate and verify that the readiness future fires.
        tx.open();
        tokio::select! {
            biased;
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
            _ = ready => println!("notified"),
        }
    }
}
