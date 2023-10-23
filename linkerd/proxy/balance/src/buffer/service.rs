use super::{future::ResponseFuture, message::Message, worker};
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

/// Adds an mpsc buffer in front of an inner service.
///
/// See the module documentation for more details.
#[derive(Debug)]
pub struct Buffer<Req, F> {
    tx: PollSender<Message<Req, F>>,
    handle: worker::SharedTerminalFailure,
}

impl<Req, F> Buffer<Req, F>
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
        let (handle, worker) = worker::spawn(rx, FAILFAST, gate_tx, resolution, pool);
        let buffer = Self {
            tx: PollSender::new(tx),
            handle,
        };
        (gate::Gate::new(gate_rx, buffer), worker)
    }

    fn get_worker_error(&self) -> Error {
        self.handle.get_error_on_closed()
    }
}

impl<Req, Rsp, F, E> Service<Req> for Buffer<Req, F>
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
            return Poll::Ready(Err(self.get_worker_error()));
        }

        // Poll the sender to acquire a permit.
        self.tx
            .poll_reserve(cx)
            .map_err(|_| self.get_worker_error())
    }

    fn call(&mut self, req: Req) -> Self::Future {
        tracing::trace!("sending request to buffer worker");

        // get the current Span so that we can explicitly propagate it to the worker
        // if we didn't do this, events on the worker related to this span wouldn't be counted
        // towards that span since the worker would have no way of entering it.
        let span = tracing::Span::current();

        // If we've made it here, then a channel permit has already been
        // acquired, so we can freely allocate a oneshot.
        let (tx, rx) = oneshot::channel();

        let t0 = time::Instant::now();
        match self.tx.send_item(Message { req, span, tx, t0 }) {
            Ok(_) => ResponseFuture::new(rx),
            // If the channel is closed, propagate the error from the worker.
            Err(_) => {
                tracing::trace!("buffer channel closed");
                ResponseFuture::failed(self.get_worker_error())
            }
        }
    }
}

impl<Req, F> Clone for Buffer<Req, F>
where
    Req: Send + 'static,
    F: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            tx: self.tx.clone(),
        }
    }
}
