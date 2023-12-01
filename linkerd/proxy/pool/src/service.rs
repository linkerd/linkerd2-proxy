use crate::{error, future::ResponseFuture, message::Message, worker, Pool};
use futures::TryStream;
use linkerd_error::{Error, Result};
use linkerd_proxy_core::Update;
use linkerd_stack::{gate, Service};
use parking_lot::RwLock;
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, time};
use tokio_util::sync::PollSender;

/// A shareable service backed by a dynamic endpoint.
#[derive(Debug)]
pub struct PoolQueue<Req, F> {
    tx: PollSender<Message<Req, F>>,
    terminal: Arc<RwLock<Option<error::TerminalFailure>>>,
}

/// Provides a copy of the terminal failure error to all handles.
#[derive(Clone, Debug, Default)]
pub(crate) struct Terminate {
    inner: Arc<RwLock<Option<error::TerminalFailure>>>,
}

// === impl Terminate ===

impl Terminate {
    pub(crate) fn send(self, error: error::TerminalFailure) {
        *self.inner.write() = Some(error);
    }
}

impl<Req, F> PoolQueue<Req, F>
where
    Req: Send + 'static,
    F: Send + 'static,
{
    pub fn spawn<T, R, P>(
        capacity: usize,
        failfast: time::Duration,
        resolution: R,
        pool: P,
    ) -> gate::Gate<Self>
    where
        T: Clone + Eq + std::fmt::Debug + Send,
        R: TryStream<Ok = Update<T>> + Send + Unpin + 'static,
        R::Error: Into<Error> + Send,
        P: Pool<T, Req, Future = F> + Send + 'static,
        P::Error: Into<Error> + Send + Sync,
        Req: Send + 'static,
    {
        let (gate_tx, gate_rx) = gate::channel();
        let (tx, rx) = mpsc::channel(capacity);
        let inner = Self::new(tx);
        let terminate = Terminate {
            inner: inner.terminal.clone(),
        };
        worker::spawn(rx, failfast, gate_tx, terminate, resolution, pool);
        gate::Gate::new(gate_rx, inner)
    }

    fn new(tx: mpsc::Sender<Message<Req, F>>) -> Self {
        Self {
            tx: PollSender::new(tx),
            terminal: Default::default(),
        }
    }

    #[inline]
    fn error_or_closed(&self) -> Error {
        (*self.terminal.read())
            .clone()
            .map(Into::into)
            .unwrap_or_else(|| error::Closed::new().into())
    }
}

impl<Req, Rsp, F, E> Service<Req> for PoolQueue<Req, F>
where
    Req: Send + 'static,
    F: Future<Output = Result<Rsp, E>> + Send + 'static,
    E: Into<Error>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = ResponseFuture<F>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let poll = self.tx.poll_reserve(cx).map_err(|_| self.error_or_closed());
        tracing::trace!(?poll);
        poll
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

impl<Req, F> Clone for PoolQueue<Req, F>
where
    Req: Send + 'static,
    F: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            terminal: self.terminal.clone(),
            tx: self.tx.clone(),
        }
    }
}
