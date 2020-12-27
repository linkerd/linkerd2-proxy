#![deny(warnings, rust_2018_idioms)]

#[cfg(feature = "tower")]
mod retain;

#[cfg(feature = "tower")]
pub use crate::retain::Retain;
use linkerd2_error::Never;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    stream::Stream,
    sync::{mpsc, watch},
};

/// Creates a drain channel.
///
/// The `Signal` is used to start a drain, and the `Watch` will be notified
/// when a drain is signaled.
pub fn channel() -> (Signal, Watch) {
    let (tx, rx) = watch::channel(());
    let (drained_tx, drained_rx) = mpsc::channel(1);

    (Signal { drained_rx, tx }, Watch { drained_tx, rx })
}

/// Send a drain command to all watchers.
///
/// When a drain is started, this returns a `Drained` future which resolves
/// when all `Watch`ers have been dropped.
#[derive(Debug)]
pub struct Signal {
    drained_rx: mpsc::Receiver<Never>,
    tx: watch::Sender<()>,
}

/// Watch for a drain command.
///
/// This wraps another future and callback to be called when drain is triggered.
#[derive(Clone, Debug)]
pub struct Watch {
    drained_tx: mpsc::Sender<Never>,
    rx: watch::Receiver<()>,
}

/// A future that resolves when all `Watch`ers have been dropped (drained).
#[pin_project]
pub struct Drained {
    #[pin]
    drained_rx: mpsc::Receiver<Never>,
}

#[must_use = "ReleaseShutdown should be dropped explicitly to release the runtime"]
#[derive(Clone, Debug)]
pub struct ReleaseShutdown(mpsc::Sender<Never>);

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
    /// Returns a `ReleaseShutdown` handle after the drain has been signaled. The
    /// handle must be dropped when a shutdown action has been completed to
    /// unblock graceful shutdown.
    pub async fn signal(mut self) -> ReleaseShutdown {
        let _ = self.rx.changed().await;
        ReleaseShutdown(self.drained_tx)
    }

    /// Return a `ReleaseShutdown` handle immediately, ignoring the release signal.
    ///
    /// This is intended to allow a task to block shutdown until it completes.
    pub fn ignore_signal(self) -> ReleaseShutdown {
        drop(self.rx);
        ReleaseShutdown(self.drained_tx)
    }

    /// Wrap a future and a callback that is triggered when drain is received.
    ///
    /// The callback receives a mutable reference to the original future, and
    /// should be used to trigger any shutdown process for it.
    pub async fn watch<A, F>(self, mut future: A, on_drain: F) -> A::Output
    where
        A: Future + Unpin,
        F: FnOnce(&mut A),
    {
        tokio::select! {
            res = &mut future => res,
            shutdown = self.signal() => {
                on_drain(&mut future);
                shutdown.release_after(future).await
            }
        }
    }
}

impl ReleaseShutdown {
    /// Releases shutdown after `future` completes.
    pub async fn release_after<F: Future>(self, future: F) -> F::Output {
        future.await
    }
}

// ===== impl Drained =====

impl Future for Drained {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match futures::ready!(self.project().drained_rx.poll_next(cx)) {
            Some(never) => match never {},
            None => Poll::Ready(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use pin_project::pin_project;
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering::SeqCst},
            Arc,
        },
        task::{Context, Poll},
    };
    use tokio::{sync::oneshot, time};

    #[pin_project]
    struct Fut {
        drained: Arc<AtomicBool>,
        #[pin]
        inner: oneshot::Receiver<()>,
    }

    impl Fut {
        pub fn new() -> (Self, oneshot::Sender<()>, Arc<AtomicBool>) {
            let drained = Arc::new(AtomicBool::new(false));
            let (tx, rx) = oneshot::channel::<()>();
            let fut = Fut {
                drained: drained.clone(),
                inner: rx,
            };
            (fut, tx, drained)
        }
    }

    impl Future for Fut {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let this = self.project();
            let _ = futures::ready!(this.inner.poll(cx));
            Poll::Ready(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watch() {
        time::pause();

        let (signal, watch) = super::channel();

        // Setup a future to be drained. When draining begins, `drained0` is
        // flipped . When `tx0` fires, the whole `watch0` future completes.
        let (fut0, tx0, drained0) = Fut::new();
        tokio::pin! {
            let watch0 = watch
                .clone()
                .watch(fut0, |f| f.drained.store(true, SeqCst));
        };

        // Setup another future to be drained.
        let (fut1, tx1, drained1) = Fut::new();
        tokio::pin! {
            let watch1 = watch.watch(fut1, |f| f.drained.store(true, SeqCst));
        }

        // Ensure that none of the futures have completed and draining hasn't
        // been signaled.
        tokio::select! {
            _ = &mut watch0 => panic!("Future terminated early"),
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = futures::future::ready(()) => {}
        }
        assert!(!drained0.load(SeqCst));
        assert!(!drained1.load(SeqCst));

        // Signal draining and ensure that none of the futures have completed.
        let mut drain = tokio::spawn(signal.drain());
        tokio::select! {
            _ = &mut watch0 => panic!("Future terminated early"),
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = &mut drain => panic!("Drain terminated early"),
            _ = time::sleep(time::Duration::from_secs(1)) => {}
        }
        // Verify that the draining callbacks were invoked.
        assert!(drained0.load(SeqCst));
        assert!(drained1.load(SeqCst));

        // Complete the first watch.
        tx0.send(()).ok().expect("must send");
        tokio::select! {
            _ = &mut watch0 => {},
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = &mut drain => panic!("Drain terminated early"),
        }

        // Complete the second watch.
        tx1.send(()).ok().expect("must send");

        // Ensure that all of our pending tasks, including the drain task,
        // complete.
        let done = async move {
            let _ = futures::join!(watch1, drain);
        };
        tokio::select! {
            _ = done => {}
            _ = time::sleep(time::Duration::from_secs(1)) => {
                panic!("Futures did not complete");
            }
        }
    }
}
