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

pub struct Permit(Option<OwnedSemaphorePermit>);

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
    pub async fn acquire(&mut self) -> Permit {
        loop {
            let state = self.0.borrow_and_update().clone();
            match state {
                State::Open => return Permit(None),
                State::Limited(sem) => match sem.acquire_owned().await {
                    Ok(permit) => return Permit(Some(permit)),
                    Err(_) => {
                        debug!("Gate closed");
                        return Permit(None);
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
    pub async fn lost(&self) {
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
                self.permit = Poll::Ready(Permit(None));
                self.inner.poll_ready(cx)
            }

            State::Shut => self.poll_acquire(cx),

            State::Limited(sem) => match sem.try_acquire_owned() {
                Ok(permit) => {
                    self.permit = Poll::Ready(Permit(Some(permit)));
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
        self.permit = match std::mem::replace(&mut self.permit, Poll::Pending) {
            Poll::Pending => panic!("poll_ready must be called first"),
            Poll::Ready(..) => Poll::Pending,
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

// === impl Permit ===

impl Drop for Permit {
    fn drop(&mut self) {
        // Permits are forgotten so the `Tx` controller can decide when to allow
        // more requests.
        if let Some(p) = self.0.take() {
            p.forget();
        }
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

    #[tokio::test]
    async fn limits() {
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
        assert_eq!(sem.available_permits(), 0);
    }
}
