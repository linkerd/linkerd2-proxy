use crate::Service;
use futures::{ready, FutureExt};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio_util::sync::ReusableBoxFuture;
use tracing::debug;

/// A middleware that alters its readiness state according to a gate channel.
pub struct Gate<S> {
    inner: S,
    rx: Rx,
    acquire: ReusableBoxFuture<'static, Permit>,
    permit: Poll<Permit>,
}

/// Observes gate state changes.
#[derive(Clone, Debug)]
pub struct Rx(watch::Receiver<State>);

/// Changes the gate state.
#[derive(Clone, Debug)]
pub struct Tx(Arc<watch::Sender<State>>);

#[derive(Clone, Debug)]
pub enum State {
    Open,
    Limited(Arc<Semaphore>),
    Shut,
}

/// Creates a new gate channel.
pub fn channel() -> (Tx, Rx) {
    let (tx, rx) = watch::channel(State::Open);
    (Tx(Arc::new(tx)), Rx(rx))
}

type Permit = Option<OwnedSemaphorePermit>;

// === impl Rx ===

impl Rx {
    /// Indicates whether the gate is open.
    pub fn state(&self) -> State {
        self.0.borrow().clone()
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state(), State::Open)
    }

    pub fn is_limited(&self) -> bool {
        matches!(self.state(), State::Limited(_))
    }

    pub fn is_shut(&self) -> bool {
        matches!(self.state(), State::Shut)
    }

    /// Waits for the gate state to change.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.0.changed().await
    }

    /// Waits for the gate state to change to be open.
    pub async fn acquire(&mut self) -> Option<OwnedSemaphorePermit> {
        loop {
            let state = self.0.borrow_and_update().clone();
            match state {
                State::Open => return None,
                State::Limited(sem) => match sem.acquire_owned().await {
                    Ok(permit) => return Some(permit),
                    Err(_) => {
                        debug!("Gate closed");
                        return None;
                    }
                },
                State::Shut => {}
            }

            if let Err(e) = self.0.changed().await {
                debug!("Gate closed: {:?}", e);
                futures::future::pending::<()>().await;
            }
        }
    }
}

// === impl Tx ===

impl Tx {
    /// Returns when all associated `Rx` clones are dropped.
    pub async fn closed(&self) {
        self.0.closed().await
    }

    /// Opens the gate.
    pub fn open(&self) {
        if self.0.send(State::Open).is_ok() {
            debug!("Gate opened");
        }
    }

    /// Opens the gate.
    pub fn limit(&self, sem: Arc<Semaphore>) {
        if self.0.send(State::Limited(sem)).is_ok() {
            debug!("Gate limited");
        }
    }

    /// Closes the gate.
    pub fn shut(&self) {
        if self.0.send(State::Shut).is_ok() {
            debug!("Gate shut");
        }
    }
}

// === impl Gate ===

impl<S> Gate<S> {
    pub fn channel(inner: S) -> (Tx, Self) {
        let (tx, rx) = channel();
        (tx, Self::new(rx, inner))
    }

    pub fn new(rx: Rx, inner: S) -> Self {
        Self {
            inner,
            rx,
            permit: Poll::Pending,
            acquire: ReusableBoxFuture::new(futures::future::pending()),
        }
    }
}

impl<S> Clone for Gate<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.rx.clone(), self.inner.clone())
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
        match self.rx.state() {
            State::Open => {
                self.permit = Poll::Ready(None);
                self.inner.poll_ready(cx)
            }

            State::Shut => self.poll_acquire(cx),

            State::Limited(sem) => match sem.try_acquire_owned() {
                Ok(permit) => {
                    self.permit = Poll::Ready(Some(permit));
                    self.inner.poll_ready(cx)
                }

                Err(TryAcquireError::NoPermits) => self.poll_acquire(cx),

                Err(TryAcquireError::Closed) => {
                    tracing::warn!("Gate closed");
                    Poll::Pending
                }
            },
        }
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.permit = match self.permit {
            Poll::Ready(..) => Poll::Pending,
            Poll::Pending => panic!("poll_ready must be called first"),
        };
        self.inner.call(req)
    }
}

impl<S> Gate<S> {
    fn poll_acquire<Req>(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>>
    where
        S: Service<Req>,
    {
        let mut rx = self.rx.clone();
        self.acquire.set(async move { rx.acquire().await });
        let permit = ready!(self.acquire.poll_unpin(cx));
        self.permit = Poll::Ready(permit);
        self.inner.poll_ready(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(1);
        tx.shut();
        assert!(gate.poll_ready().is_pending());

        tx.open();
        assert!(gate.poll_ready().is_ready());
    }

    #[tokio::test]
    async fn gate_polls_inner() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

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
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        // Start with a shut gate on an available inner service.
        handle.allow(1);
        tx.shut();

        // Wait for the gated service to become ready.
        assert!(gate.poll_ready().is_pending());

        // Open the gate and verify that the readiness future fires.
        tx.open();
        assert!(gate.poll_ready().is_ready());
    }

    #[tokio::test]
    async fn channel_closes() {
        let (tx, rx) = channel();
        let mut closed = tokio_test::task::spawn(tx.closed());
        assert!(closed.poll().is_pending());
        drop(rx);
        assert!(closed.poll().is_ready());
    }

    #[tokio::test]
    async fn channel_closes_after_clones() {
        let (tx, rx0) = channel();
        let mut closed = tokio_test::task::spawn(tx.closed());
        let rx1 = rx0.clone();
        assert!(closed.poll().is_pending());
        drop(rx0);
        assert!(closed.poll().is_pending());
        drop(rx1);
        assert!(closed.poll().is_ready());
    }

    #[tokio::test]
    async fn channel_closes_after_clones_reordered() {
        let (tx, rx0) = channel();
        let mut closed = tokio_test::task::spawn(tx.closed());
        let rx1 = rx0.clone();
        assert!(closed.poll().is_pending());
        drop(rx1);
        assert!(closed.poll().is_pending());
        drop(rx0);
        assert!(closed.poll().is_ready());
    }
}
