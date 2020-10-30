use crate::error::Closed;
use crate::InFlight;
use futures::ready;
use linkerd2_error::Error;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{fmt, future::Future, mem, pin::Pin};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

pub struct Buffer<Req, Rsp> {
    /// The queue on which in-flight requests are sent to the inner service.
    tx: mpsc::UnboundedSender<InFlight<Req, Rsp>>,
    semaphore: Arc<Semaphore>,
    state: State,
}

enum State {
    Waiting(Pin<Box<dyn Future<Output = OwnedSemaphorePermit> + Send + Sync>>),
    Acquired(OwnedSemaphorePermit),
    Empty,
}

// === impl Buffer ===

impl<Req, Rsp> Buffer<Req, Rsp> {
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<InFlight<Req, Rsp>>,
        semaphore: Arc<Semaphore>,
    ) -> Self {
        Self {
            tx,
            semaphore,
            state: State::Empty,
        }
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
        loop {
            self.state = match self.state {
                State::Empty => State::Waiting(Box::pin(self.semaphore.clone().acquire_owned())),
                State::Waiting(ref mut f) => State::Acquired(ready!(Pin::new(f).poll(cx))),
                State::Acquired(_) if self.tx.is_closed() => {
                    return Poll::Ready(Err(Closed(()).into()))
                }
                State::Acquired(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    fn call(&mut self, request: Req) -> Self::Future {
        let _permit = match mem::replace(&mut self.state, State::Empty) {
            State::Acquired(permit) => permit,
            state => panic!("poll_ready must be called (actual state={:?})", state),
        };
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(InFlight {
                request,
                tx,
                _permit,
            })
            .ok()
            .expect("poll_ready must be called");
        Box::pin(async move { rx.await.map_err(|_| Closed(()))??.await })
    }
}

impl<Req, Rsp> Clone for Buffer<Req, Rsp> {
    fn clone(&self) -> Self {
        Self::new(self.tx.clone(), self.semaphore.clone())
    }
}

// === impl State ===

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(
            match self {
                State::Acquired(_) => "State::Acquired(..)",
                State::Waiting(_) => "State::Waiting(..)",
                State::Empty => "State::Empty",
            },
            f,
        )
    }
}
