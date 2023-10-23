use crate::{error, future::ResponseFuture, message::Message, worker, Pool};
use futures::TryStream;
use linkerd_error::{Error, Result};
use linkerd_proxy_core::Update;
use linkerd_stack::{gate, Service};
use std::{
    future::Future,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, time};
use tokio_util::sync::PollSender;

/// A shareable service backed by a dynamic set of endpoints.
#[derive(Debug)]
pub struct PoolQ<Req, F> {
    tx: PollSender<Message<Req, F>>,
    terminal_failure: worker::SharedTerminalFailure,
}

impl<Req, F> PoolQ<Req, F>
where
    Req: Send + 'static,
    F: Send + 'static,
{
    pub fn spawn<T, R, P>(
        failfast: time::Duration,
        resolution: R,
        pool: P,
        bound: usize,
    ) -> gate::Gate<Self>
    where
        T: Clone + Eq + std::fmt::Debug + Send,
        R: TryStream<Ok = Update<T>> + Send + Unpin + 'static,
        R::Error: Into<Error> + Send,
        P: Pool<T> + Service<Req, Future = F> + Send + 'static,
        P::Error: Into<Error> + Send + Sync,
        Req: Send + 'static,
    {
        let (gate_tx, gate_rx) = gate::channel();
        let (tx, rx) = mpsc::channel(bound);
        let (terminal_failure, _task) = worker::spawn(rx, failfast, gate_tx, resolution, pool);
        gate::Gate::new(gate_rx, Self::new(tx, terminal_failure))
    }

    fn new(
        tx: mpsc::Sender<Message<Req, F>>,
        terminal_failure: worker::SharedTerminalFailure,
    ) -> Self {
        Self {
            tx: PollSender::new(tx),
            terminal_failure,
        }
    }

    #[inline]
    fn error_or_closed(&self) -> Error {
        self.terminal_failure
            .get()
            .map(Into::into)
            .unwrap_or_else(|| error::Closed::new().into())
    }
}

impl<Req, Rsp, F, E> Service<Req> for PoolQ<Req, F>
where
    Req: Send + 'static,
    F: Future<Output = Result<Rsp, E>> + Send + 'static,
    E: Into<Error>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = ResponseFuture<F>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // First, check if the worker is still alive.
        if self.tx.is_closed() {
            // If the inner service has errored, then we error here.
            return Poll::Ready(Err(self.error_or_closed()));
        }

        // Poll the sender to acquire a permit.
        self.tx.poll_reserve(cx).map_err(|_| self.error_or_closed())
    }

    fn call(&mut self, req: Req) -> Self::Future {
        tracing::trace!("Sending request to worker");
        let (msg, rx) = Message::channel(req);
        if self.tx.send_item(msg).is_err() {
            // The channel closed since poll_ready was called, so propagate the
            // failure in the response future.
            return ResponseFuture::failed(self.error_or_closed());
        }
        ResponseFuture::new(rx)
    }
}

impl<Req, F> Clone for PoolQ<Req, F>
where
    Req: Send + 'static,
    F: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            terminal_failure: self.terminal_failure.clone(),
            tx: self.tx.clone(),
        }
    }
}
