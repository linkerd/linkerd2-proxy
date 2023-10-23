use super::{error, future::ResponseFuture, message::Message, worker};
use futures_util::TryStream;
use linkerd_error::{Error, Result};
use linkerd_proxy_core::{Pool, Update};
use linkerd_stack::{gate, Service};
use std::{
    future::Future,
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time,
};
use tokio_util::sync::PollSender;

#[derive(Debug)]
pub struct Queue<Req, F> {
    tx: PollSender<Message<Req, F>>,
    terminal_failure: worker::SharedTerminalFailure,
}

impl<Req, F> Queue<Req, F>
where
    F: Send + 'static,
{
    pub fn spawn<T, R, P>(
        resolution: R,
        pool: P,
        bound: usize,
    ) -> (gate::Gate<Self>, JoinHandle<Result<()>>)
    where
        T: Clone + Eq + std::fmt::Debug + Send,
        R: TryStream<Ok = Update<T>> + Send + Unpin,
        R::Error: Into<Error> + Send,
        P: Pool<T> + Service<Req, Future = F> + Send + 'static,
        P::Future: Send,
        P::Error: Into<Error> + Send + Sync,
        Req: Send + 'static,
    {
        let (gate_tx, gate_rx) = gate::channel();
        let (tx, rx) = mpsc::channel(bound);
        const FAILFAST: time::Duration = time::Duration::from_secs(10);
        let (terminal_failure, worker) = worker::spawn(rx, FAILFAST, gate_tx, resolution, pool);
        let service = Self {
            tx: PollSender::new(tx),
            terminal_failure,
        };
        (gate::Gate::new(gate_rx, service), worker)
    }

    #[inline]
    fn error_or_closed(&self) -> Error {
        self.terminal_failure
            .get()
            .map(Into::into)
            .unwrap_or_else(|| error::Closed::new().into())
    }
}

impl<Req, Rsp, F, E> Service<Req> for Queue<Req, F>
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

impl<Req, F> Clone for Queue<Req, F>
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
