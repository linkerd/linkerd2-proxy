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
    ///
    /// The semaphore is closed when the gate state changes.
    Limited(Arc<Semaphore>),

    /// The gate is shut and no requests are to be admitted.
    Shut,
}

#[derive(Debug, thiserror::Error)]
#[error("gate closed")]
pub struct Closed(());

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
    pub async fn opened_for_test(&mut self) -> Option<OwnedSemaphorePermit> {
        self.opened().await
    }

    /// Waits for the gate state to change to be open. Returns a permit if the
    /// gate is in a limited state.
    async fn opened(&mut self) -> Option<OwnedSemaphorePermit> {
        loop {
            let state = self.0.borrow_and_update().clone();
            match state {
                State::Open => return None,
                State::Shut => {}

                // If the state changes while we're waiting for a permit, the
                // semaphore is closed.
                State::Limited(sem) => match sem.acquire_owned().await {
                    Ok(permit) => return Some(permit),
                    Err(_closed) => {
                        // When the semaphore is closed, continue waiting for
                        // the state to change.
                        debug!("Semaphore closed");
                    }
                },
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
    pub fn open(&self) -> Result<(), Closed> {
        if self.0.is_closed() {
            return Err(Closed(()));
        }
        match self.0.send_replace(State::Open) {
            State::Open => {}
            State::Limited(lim) => {
                debug!("Limited => Open");
                lim.close();
            }
            State::Shut => {
                debug!("Shut => Open");
            }
        }
        Ok(())
    }

    /// Limits the gate, returning a semaphore that can be used to control the
    /// limit. If the inner state changes, this semaphore will be closed and
    /// become unusable. When the gate is already in a Limited state, the prior
    /// semaphore is closed and a new one is created with the provided number of
    /// permits.
    pub fn limit(&self, permits: usize) -> Result<Arc<Semaphore>, Closed> {
        if self.0.is_closed() {
            return Err(Closed(()));
        }
        let sem = Arc::new(Semaphore::new(permits));
        match self.0.send_replace(State::Limited(sem.clone())) {
            State::Open => {
                debug!("Open => Limited");
            }
            State::Limited(lim) => {
                debug!("Limited => Limited; replaced semaphore");
                lim.close();
            }
            State::Shut => {
                debug!("Shut => Limited");
            }
        }
        Ok(sem)
    }

    /// Closes the gate.
    pub fn shut(&self) -> Result<(), Closed> {
        if self.0.is_closed() {
            return Err(Closed(()));
        }
        match self.0.send_replace(State::Shut) {
            State::Shut => {}
            State::Open => {
                debug!("Open => Shut");
            }
            State::Limited(lim) => {
                debug!("Limited => Shut");
                lim.close();
            }
        }
        Ok(())
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
        // If we previously polled to ready and acquired a permit, clear it so
        // we can reestablish readiness without holding it.
        self.permit = Poll::Pending;
        let permit = ready!(self.poll_acquire(cx));
        ready!(self.inner.poll_ready(cx))?;
        tracing::trace!("Acquired permit");
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
            self.acquire.set(async move { rx.opened().await });
        }

        let permit = ready!(self.acquire.poll_unpin(cx));
        self.acquiring = false;
        Poll::Ready(permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio_test::{assert_pending, assert_ready, task};

    #[tokio::test]
    async fn gate() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(1);
        tx.shut().unwrap();
        assert_pending!(gate.poll_ready());

        tx.open().unwrap();
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn gate_polls_inner() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        handle.allow(0);
        assert_pending!(gate.poll_ready());

        tx.shut().unwrap();
        assert_pending!(gate.poll_ready());

        tx.open().unwrap();
        assert_pending!(gate.poll_ready());

        handle.allow(1);
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn gate_repolls_back_to_pending() {
        let (tx, rx) = channel();
        let pending = Arc::new(AtomicBool::new(false));
        let (mut gate, mut handle) = {
            struct Svc<S>(S, Arc<AtomicBool>);
            impl<Req, S: Service<Req>> Service<Req> for Svc<S> {
                type Response = S::Response;
                type Error = S::Error;
                type Future = S::Future;
                fn poll_ready(
                    &mut self,
                    cx: &mut std::task::Context<'_>,
                ) -> std::task::Poll<Result<(), Self::Error>> {
                    if self.1.load(std::sync::atomic::Ordering::Relaxed) {
                        return Poll::Pending;
                    }
                    self.0.poll_ready(cx)
                }
                fn call(&mut self, req: Req) -> Self::Future {
                    self.0.call(req)
                }
            }

            let pending = pending.clone();
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| {
                Gate::new(rx.clone(), Svc(inner, pending.clone()))
            })
        };

        tx.open().unwrap();
        handle.allow(1);
        assert_ready!(gate.poll_ready()).expect("ok");

        pending.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_pending!(gate.poll_ready());

        pending.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn notifies_on_open() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        // Start with a shut gate on an available inner service.
        handle.allow(1);
        tx.shut().unwrap();

        // Wait for the gated service to become ready.
        assert_pending!(gate.poll_ready());

        // Open the gate and verify that the readiness future fires.
        tx.open().unwrap();
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn notifies_on_open_when_limited() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        // Start with a shut gate on an available inner service.
        handle.allow(1);

        let _sem = tx.limit(0).unwrap();

        // Wait for the gated service to become ready.
        assert_pending!(gate.poll_ready());

        // Open the gate and verify that the readiness future fires.
        tx.open().unwrap();
        assert_ready!(gate.poll_ready()).expect("ok");
    }

    #[tokio::test]
    async fn notifies_on_shut_and_open_when_limited() {
        let (tx, rx) = channel();
        let (mut gate, mut handle) =
            tower_test::mock::spawn_with::<(), (), _, _>(move |inner| Gate::new(rx.clone(), inner));

        // Start with a limited gate on an available inner service.
        handle.allow(1);
        let _sem = tx.limit(0).unwrap();
        assert_pending!(gate.poll_ready());

        // Shut it.
        tx.shut().unwrap();
        assert_pending!(gate.poll_ready());

        // Open the gate and verify that the readiness future fires.
        tx.open().unwrap();
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
        let sem = tx.limit(1).unwrap();
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
