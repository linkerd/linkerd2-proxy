#![deny(warnings, rust_2018_idioms)]

use futures::{future::Shared, FutureExt};
use linkerd2_error::Never;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};

/// Creates a drain channel.
///
/// The `Signal` is used to start a drain, and the `Watch` will be notified
/// when a drain is signaled.
pub fn channel() -> (Signal, Watch) {
    let (tx, rx) = oneshot::channel();
    let (drained_tx, drained_rx) = mpsc::channel(1);

    // Since `FutureExt::shared` requires the future's `Output` type to
    // implement `Clone`, and `oneshot::RecvError` does not, just map the
    // `Output` to `()`. The behavior is the same regardless of whether the
    // drain is explicitly signalled or the `Signal` type is thrown away, so
    // we don't care whether this is an error or not.
    let rx = rx.map((|_| ()) as fn(_) -> _).shared();

    (Signal { drained_rx, tx }, Watch { drained_tx, rx })
}

/// Send a drain command to all watchers.
///
/// When a drain is started, this returns a `Drained` future which resolves
/// when all `Watch`ers have been dropped.
#[derive(Debug)]
pub struct Signal {
    drained_rx: mpsc::Receiver<Never>,
    tx: oneshot::Sender<()>,
}

/// Watch for a drain command.
///
/// This wraps another future and callback to be called when drain is triggered.
#[pin_project]
#[derive(Clone, Debug)]
pub struct Watch {
    #[pin]
    drained_tx: mpsc::Sender<Never>,
    rx: Shared<
        futures::future::Map<
            oneshot::Receiver<()>,
            fn(Result<(), tokio::sync::oneshot::error::RecvError>) -> (),
        >,
    >,
}

/// A future that resolves when all `Watch`ers have been dropped (drained).
#[pin_project]
pub struct Drained {
    #[pin]
    drained_rx: mpsc::Receiver<Never>,
}

// ===== impl Signal =====

impl Signal {
    /// Start the draining process.
    ///
    /// A signal is sent to all futures watching for the signal. A new future
    /// is returned from this method that resolves when all watchers have
    /// completed.
    pub fn drain(self) -> Drained {
        let _ = self.tx.send(());
        Drained {
            drained_rx: self.drained_rx,
        }
    }
}

// ===== impl Watch =====

impl Watch {
    pub async fn into_future(self) {
        self.rx.await;
    }

    /// Wrap a future to count it against the completion of the `Drained`
    /// future that corresponds to this `Watch`.
    ///
    /// Unlike `Watch::watch`, this method does not take a callback that is
    /// triggered on drain; the wrapped future will simply be allowed to
    /// complete. However, like `Watch::watch`, the `Drained` future returned
    /// by calling `drain` on the corresponding `Signal` will not complete until
    /// the wrapped future finishes.
    pub async fn after<A>(self, future: A) -> A::Output
    where
        A: Future,
    {
        let res = future.await;
        drop(self);
        res
    }
}

// ===== impl Drained =====

impl Future for Drained {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match futures::ready!(self.project().drained_rx.poll_recv(cx)) {
            Some(never) => match never {},
            None => Poll::Ready(()),
        }
    }
}
