use linkerd_error::Error;
use linkerd_proxy_core::Update;
use parking_lot::Mutex;
use std::{
    sync::Arc,
    task::{Context, Poll, Waker},
};
use tokio::sync::mpsc;
use tower_test::mock;

pub fn pool<T, Req, Rsp>() -> (MockPool<T, Req, Rsp>, PoolHandle<T, Req, Rsp>) {
    let state = Arc::new(Mutex::new(State {
        poll: Poll::Ready(Ok(())),
        waker: None,
    }));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let (mock, svc) = mock::pair();
    let h = PoolHandle {
        rx: updates_rx,
        state: state.clone(),
        svc,
    };
    let p = MockPool {
        tx: updates_tx,
        state,
        svc: mock,
    };
    (p, h)
}

pub struct MockPool<T, Req, Rsp> {
    tx: mpsc::UnboundedSender<Update<T>>,
    state: Arc<Mutex<State>>,
    svc: mock::Mock<Req, Rsp>,
}

pub struct PoolHandle<T, Req, Rsp> {
    state: Arc<Mutex<State>>,
    pub rx: mpsc::UnboundedReceiver<Update<T>>,
    pub svc: mock::Handle<Req, Rsp>,
}

struct State {
    poll: Poll<Result<(), PoolError>>,
    waker: Option<Waker>,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("mock pool error")]
pub struct PoolError;

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("mock resolution error")]
pub struct ResolutionError;

impl<T, Req, Rsp> crate::Pool<T, Req> for MockPool<T, Req, Rsp> {
    fn update_pool(&mut self, update: Update<T>) {
        self.tx.send(update).ok().unwrap();
    }

    fn poll_pool(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut s = self.state.lock();
        s.waker.replace(cx.waker().clone());
        s.poll.map_err(Into::into)
    }
}

impl<T, Req, Rsp> linkerd_stack::Service<Req> for MockPool<T, Req, Rsp> {
    type Response = Rsp;
    type Error = Error;
    type Future = mock::future::ResponseFuture<Rsp>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Poll::Ready(res) = self.svc.poll_ready(cx) {
            return Poll::Ready(res);
        }
        // Drive the pool when the service isn't ready.
        let _ = crate::Pool::poll_pool(self, cx)?;
        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.svc.call(req)
    }
}

impl<T, Req, Rsp> PoolHandle<T, Req, Rsp> {
    pub fn set_poll(&self, poll: Poll<Result<(), PoolError>>) {
        let mut s = self.state.lock();
        s.poll = poll;
        if let Some(w) = s.waker.take() {
            tracing::trace!("Wake");
            w.wake();
        } else {
            tracing::trace!("No waker");
        }
    }
}
