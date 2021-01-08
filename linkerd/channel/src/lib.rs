use futures::{future, ready, Stream};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::{fmt, future::Future, mem, pin::Pin};
use tokio::sync::{mpsc, OwnedSemaphorePermit as Permit, Semaphore};

use self::error::{SendError, TrySendError};
pub use tokio::sync::mpsc::error;

/// Returns a new pollable, bounded MPSC channel.
///
/// Unlike `tokio::sync`'s `MPSC` channel, this channel exposes a `poll_ready`
/// function, at the cost of an allocation when driving it to readiness.
pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0, "mpsc bounded channel requires buffer > 0");
    let semaphore = Arc::new(Semaphore::new(buffer));
    let (tx, rx) = mpsc::unbounded_channel();
    let rx = Receiver {
        rx,
        semaphore: Arc::downgrade(&semaphore),
        buffer,
    };
    let tx = Sender {
        tx,
        semaphore,
        state: State::Empty,
    };
    (tx, rx)
}

/// A bounded, pollable MPSC sender.
///
/// This is similar to Tokio's bounded MPSC channel's `Sender` type, except that
/// it exposes a `poll_ready` function, at the cost of an allocation when
/// driving it to readiness.
pub struct Sender<T> {
    tx: mpsc::UnboundedSender<(T, Permit)>,
    semaphore: Arc<Semaphore>,
    state: State,
}

/// A bounded MPSC receiver.
///
/// This is similar to Tokio's bounded MPSC channel's `Receiver` type.
pub struct Receiver<T> {
    rx: mpsc::UnboundedReceiver<(T, Permit)>,
    semaphore: Weak<Semaphore>,
    buffer: usize,
}

enum State {
    Waiting(Pin<Box<dyn Future<Output = Permit> + Send + Sync>>),
    Acquired(Permit),
    Empty,
}

impl<T> Sender<T> {
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError<()>>> {
        loop {
            self.state = match self.state {
                State::Empty => State::Waiting(Box::pin(self.semaphore.clone().acquire_owned())),
                State::Waiting(ref mut f) => State::Acquired(ready!(Pin::new(f).poll(cx))),
                State::Acquired(_) if self.tx.is_closed() => {
                    return Poll::Ready(Err(SendError(())))
                }
                State::Acquired(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    pub async fn ready(&mut self) -> Result<(), SendError<()>> {
        future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    pub fn try_send(&mut self, value: T) -> Result<(), TrySendError<T>> {
        if self.tx.is_closed() {
            return Err(TrySendError::Closed(value));
        }
        self.state = match mem::replace(&mut self.state, State::Empty) {
            // Have we previously acquired a permit?
            State::Acquired(permit) => {
                self.send2(value, permit);
                return Ok(());
            }
            // Okay, can we acquire a permit now?
            State::Empty => {
                if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
                    self.send2(value, permit);
                    return Ok(());
                }
                State::Empty
            }
            state => state,
        };
        Err(TrySendError::Full(value))
    }

    pub async fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        if self.ready().await.is_err() {
            return Err(SendError(value));
        }
        match mem::replace(&mut self.state, State::Empty) {
            State::Acquired(permit) => {
                self.send2(value, permit);
                Ok(())
            }
            state => panic!("unexpected state after poll_ready: {:?}", state),
        }
    }

    fn send2(&mut self, value: T, permit: Permit) {
        self.tx.send((value, permit)).ok().expect("was not closed");
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            semaphore: self.semaphore.clone(),
            state: State::Empty,
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("message_type", &std::any::type_name::<T>())
            .field("state", &self.state)
            .field("semaphore", &self.semaphore)
            .finish()
    }
}

// === impl Receiver ===

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await.map(|(t, _)| t)
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let res = ready!(Pin::new(&mut self.rx).poll_recv(cx));
        Poll::Ready(res.map(|(t, _)| t))
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().poll_recv(cx)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if let Some(semaphore) = self.semaphore.upgrade() {
            // Close the buffer by releasing any senders waiting on channel capacity.
            // If more than `usize::MAX >> 3` permits are added to the semaphore, it
            // will panic.
            const MAX: usize = std::usize::MAX >> 4;
            semaphore.add_permits(MAX - self.buffer - semaphore.available_permits());
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("message_type", &std::any::type_name::<T>())
            .field("semaphore", &self.semaphore)
            .finish()
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
