use crate::Service;
use futures::{ready, FutureExt};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::ReusableBoxFuture;
use tracing::debug;

/// A middleware that alters its readiness state according to a gate channel.
pub struct Gate<S> {
    inner: S,
    rx: Rx,
    acquire: ReusableBoxFuture<'static, Option<OwnedSemaphorePermit>>,
    acquiring: bool,
    permit: Poll<Option<OwnedSemaphorePermit>>,
}

/// Observes gate state changes.
#[derive(Clone, Debug)]
pub struct Rx(watch::Receiver<State>);

/// Changes the gate state.
#[derive(Clone, Debug)]
pub struct Tx(Arc<watch::Sender<State>>);

#[derive(Clone, Debug)]
pub enum State {
    /// The gate is open and unlimited.
    Open,

    /// The gate is limited by the provided semaphore. Permits are forgotten as
    /// requests are processed so that this semaphore can specify a limit on the
    /// number of requests to be admitted by the [`Gate`].
    Limited(Arc<Semaphore>),

    /// The gate is shut and no requests are to be admitted.
    Shut,
}

/// Creates a new gate channel.
pub fn channel() -> (Tx, Rx) {
    let (tx, rx) = watch::channel(State::Open);
    (Tx(Arc::new(tx)), Rx(rx))
}

// === impl Rx ===

impl Rx {
    /// Returns a clone of the current state.
    pub fn state(&self) -> State {
        self.0.borrow().clone()
    }

    /// Indicates whether the gate is open.
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
    ///
    /// This is intended for testing purposes.
    #[cfg(feature = "test-util")]
    pub async fn acquire_for_test(&mut self) -> Option<OwnedSemaphorePermit> {
        self.acquire().await
    }

    /// Waits for the gate state to change to be open.
    async fn acquire(&mut self) -> Option<OwnedSemaphorePermit> {
        loop {
            let state = self.0.borrow_and_update().clone();
            match state {
                State::Open => return None,
                State::Limited(sem) => match sem.acquire_owned().await {
                    Ok(permit) => return Some(permit),
                    Err(_closed) => {
                        // When the semaphore is closed, continue waiting for
                        // the state to change.
                        debug!("Semaphore closed");
                    }
                },
                State::Shut => {}
            }

            if let Err(e) = self.0.changed().await {
                // If the `Tx` is dropped, then no further state changes can
                // occur, so we simply remain in an unavilable state.
                debug!("Gate closed: {:?}", e);
                futures::future::pending::<()>().await;
            }
        }
    }
}

// === impl Tx ===

impl Tx {
    /// Returns when all associated `Rx` clones are dropped.
    pub async fn lost(&self) {
        self.0.closed().await
    }

    /// Opens the gate.
    pub fn open(&self) {
        if self.0.send(State::Open).is_ok() {
            debug!("Gate opened");
        }
    }

    /// Limits the gate with the provided semaphore.
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
            acquiring: false,
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
        let permit = ready!(self.poll_acquire(cx));
        ready!(self.inner.poll_ready(cx))?;
        self.permit = Poll::Ready(permit);
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        match std::mem::replace(&mut self.permit, Poll::Pending) {
            Poll::Pending => panic!("poll_ready must be called first"),
            Poll::Ready(Some(permit)) => permit.forget(),
            Poll::Ready(None) => {}
        };

        self.inner.call(req)
    }
}

impl<S> Gate<S> {
    fn poll_acquire<Req>(&mut self, cx: &mut Context<'_>) -> Poll<Option<OwnedSemaphorePermit>>
    where
        S: Service<Req>,
    {
        if !self.acquiring {
            match self.rx.state() {
                State::Open => return Poll::Ready(None),
                State::Limited(sem) => {
                    if let Ok(permit) = sem.try_acquire_owned() {
                        return Poll::Ready(Some(permit));
                    }
                    // If the semaphore is closed or at capacity, wait until
                    // something changes.
                }
                // If the gate is shut, wait for the state to change.
                State::Shut => {}
            }

            self.acquiring = true;
            let mut rx = self.rx.clone();
            self.acquire.set(async move { rx.acquire().await });
        }

        let permit = ready!(self.acquire.poll_unpin(cx));
        self.acquiring = false;
        Poll::Ready(permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::{assert_pending, assert_ready, task};

    #[tokio::test]
    async fn gate() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(1);
        tx.shut();
        assert_pending!(gate.poll_ready());

        tx.open();
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn gate_polls_inner() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(0);
        assert_pending!(gate.poll_ready());

        tx.shut();
        assert_pending!(gate.poll_ready());

        tx.open();
        assert_pending!(gate.poll_ready());

        handle.allow(1);
        assert_ready!(gate.poll_ready()).expect("ok");
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
        assert_pending!(gate.poll_ready());

        // Open the gate and verify that the readiness future fires.
        tx.open();
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn channel_closes() {
        let (tx, rx) = channel();
        let mut closed = task::spawn(tx.lost());
        assert_pending!(closed.poll());
        drop(rx);
        assert_ready!(closed.poll());
    }

    #[tokio::test]
    async fn channel_closes_after_clones() {
        let (tx, rx0) = channel();
        let mut closed = task::spawn(tx.lost());
        let rx1 = rx0.clone();
        assert_pending!(closed.poll());
        drop(rx0);
        assert_pending!(closed.poll());
        drop(rx1);
        assert_ready!(closed.poll());
    }

    #[tokio::test]
    async fn channel_closes_after_clones_reordered() {
        let (tx, rx0) = channel();
        let mut closed = task::spawn(tx.lost());
        let rx1 = rx0.clone();
        assert_pending!(closed.poll());
        drop(rx1);
        assert_pending!(closed.poll());
        drop(rx0);
        assert_ready!(closed.poll());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn limits() {
        let _trace = linkerd_tracing::test::trace_init();

        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(2);
        let sem = Arc::new(Semaphore::new(0));
        tx.limit(sem.clone());

        assert_pending!(gate.poll_ready());
        sem.add_permits(1);
        assert_ready!(gate.poll_ready()).expect("ok");
        assert_eq!(sem.available_permits(), 0);
        let rsp = handle
            .next_request()
            .map(|tx| tx.unwrap().1.send_response(()));
        let (res, _) = tokio::join!(gate.call(()), rsp);
        res.expect("ok");

        assert_pending!(gate.poll_ready());
        sem.add_permits(1);
        assert_ready!(gate.poll_ready()).expect("ok");
        assert_eq!(sem.available_permits(), 0);
        let rsp = handle
            .next_request()
            .map(|tx| tx.unwrap().1.send_response(()));
        let (res, _) = tokio::join!(gate.call(()), rsp);
        res.expect("ok");

        assert_pending!(gate.poll_ready());
        sem.add_permits(1);
        assert_pending!(gate.poll_ready());
        assert_eq!(sem.available_permits(), 1);

        let mut gate2 = gate.clone();
        drop(gate);
        assert_pending!(gate2.poll_ready());
        assert_eq!(sem.available_permits(), 1);
    }
}
