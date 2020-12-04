use crate::error::Closed;
use crate::InFlight;
use linkerd2_channel as mpsc;
use linkerd2_error::Error;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tokio::sync::oneshot;

pub struct Buffer<Req, Rsp> {
    /// The queue on which in-flight requests are sent to the inner service.
    tx: mpsc::Sender<InFlight<Req, Rsp>>,
}

// === impl Buffer ===

impl<Req, Rsp> Buffer<Req, Rsp> {
    pub(crate) fn new(tx: mpsc::Sender<InFlight<Req, Rsp>>) -> Self {
        Self { tx }
    }
}

impl<Req, Rsp> tower::Service<Req> for Buffer<Req, Rsp>
where
    Rsp: Send + 'static,
{
    type Response = Rsp;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(InFlight { request, tx })
            .expect("poll_ready must be called");
        Box::pin(async move { rx.await.map_err(|_| Closed(()))??.await })
    }
}

impl<Req, Rsp> Clone for Buffer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone())
    }
}
