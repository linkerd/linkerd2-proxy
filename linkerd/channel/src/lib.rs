use futures::{future, ready, Stream};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::{fmt, pin::Pin};
use tokio::sync::{mpsc, OwnedSemaphorePermit as Permit, Semaphore};
use tokio_util::sync::PollSemaphore;

use self::error::{SendError, TrySendError};
pub use tokio::sync::mpsc::error;

pub mod into_stream;

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
    };
    let tx = Sender {
        tx,
        semaphore: PollSemaphore::new(semaphore),
        permit: None,
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
    semaphore: PollSemaphore,
    permit: Option<Permit>,
}

/// A bounded MPSC receiver.
///
/// This is similar to Tokio's bounded MPSC channel's `Receiver` type.
pub struct Receiver<T> {
    rx: mpsc::UnboundedReceiver<(T, Permit)>,
    semaphore: Weak<Semaphore>,
}

impl<T> Sender<T> {
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError<()>>> {
        if self.permit.is_some() {
            return Poll::Ready(Ok(()));
        }
        let permit = ready!(self.semaphore.poll_acquire(cx)).ok_or(SendError(()))?;
        self.permit = Some(permit);
        Poll::Ready(Ok(()))
    }

    pub async fn ready(&mut self) -> Result<(), SendError<()>> {
        future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    pub fn try_send(&mut self, value: T) -> Result<(), TrySendError<T>> {
        if self.tx.is_closed() {
            return Err(TrySendError::Closed(value));
        }

        // Have we previously acquired a permit?
        if let Some(permit) = self.permit.take() {
            self.send2(value, permit);
            return Ok(());
        }

        // Okay, can we acquire a permit now?
        if let Ok(permit) = self.semaphore.clone_inner().try_acquire_owned() {
            self.send2(value, permit);
            return Ok(());
        }

        Err(TrySendError::Full(value))
    }

    pub async fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        if self.ready().await.is_err() {
            return Err(SendError(value));
        }

        let permit = self
            .permit
            .take()
            .expect("permit must be acquired if poll_ready returned ready");
        self.send2(value, permit);
        Ok(())
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
            permit: None,
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("message_type", &std::any::type_name::<T>())
            .field("permit", &self.permit)
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
            semaphore.close();
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
