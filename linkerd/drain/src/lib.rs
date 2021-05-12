#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

#[cfg(feature = "tower")]
mod retain;

#[cfg(feature = "tower")]
pub use crate::retain::Retain;
use std::future::Future;
use tokio::sync::{mpsc, watch};

/// Creates a drain channel.
///
/// The `Signal` is used to start a drain, and the `Watch` will be notified
/// when a drain is signaled.
pub fn channel() -> (Signal, Watch) {
    let (signal_tx, signal_rx) = watch::channel(());
    let (drained_tx, drained_rx) = mpsc::channel(1);

    let signal = Signal {
        drained_rx,
        signal_tx,
    };
    let watch = Watch {
        drained_tx,
        signal_rx,
    };
    (signal, watch)
}

enum Never {}

/// Send a drain command to all watchers.
#[derive(Debug)]
pub struct Signal {
    drained_rx: mpsc::Receiver<Never>,
    signal_tx: watch::Sender<()>,
}

/// Watch for a drain command.
///
/// All `Watch` instances must be dropped for a `Signal::signal` call to
/// complete.
#[derive(Clone, Debug)]
pub struct Watch {
    drained_tx: mpsc::Sender<Never>,
    signal_rx: watch::Receiver<()>,
}

#[must_use = "ReleaseShutdown should be dropped explicitly to release the runtime"]
#[derive(Clone, Debug)]
pub struct ReleaseShutdown(mpsc::Sender<Never>);

// === impl Signal ===

impl Signal {
    /// Asynchronously signals all watchers to begin draining and waits for all
    /// handles to be dropped.
    pub async fn drain(mut self) {
        // Update the state of the signal watch so that all watchers are observe
        // the change.
        let _ = self.signal_tx.send(());

        // Wait for all watchers to release their drain handle.
        match self.drained_rx.recv().await {
            None => {}
            Some(n) => match n {},
        }
    }
}

// === impl Watch ===

impl Watch {
    /// Returns a `ReleaseShutdown` handle after the drain has been signaled. The
    /// handle must be dropped when a shutdown action has been completed to
    /// unblock graceful shutdown.
    pub async fn signaled(mut self) -> ReleaseShutdown {
        // This future completes once `Signal::signal` has been invoked so that
        // the channel's state is updated.
        let _ = self.signal_rx.changed().await;

        // Return a handle that holds the drain channel, so that the signal task
        // is only notified when all handles have been dropped.
        ReleaseShutdown(self.drained_tx)
    }

    /// Return a `ReleaseShutdown` handle immediately, ignoring the release signal.
    ///
    /// This is intended to allow a task to block shutdown until it completes.
    pub fn ignore_signaled(self) -> ReleaseShutdown {
        drop(self.signal_rx);
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
            shutdown = self.signaled() => {
                on_drain(&mut future);
                shutdown.release_after(future).await
            }
        }
    }
}

impl ReleaseShutdown {
    /// Releases shutdown after `future` completes.
    pub async fn release_after<F: Future>(self, future: F) -> F::Output {
        let res = future.await;
        drop(self.0);
        res
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
        notified: Arc<AtomicBool>,
        #[pin]
        inner: oneshot::Receiver<()>,
    }

    impl Fut {
        pub fn new() -> (Self, oneshot::Sender<()>, Arc<AtomicBool>) {
            let notified = Arc::new(AtomicBool::new(false));
            let (tx, rx) = oneshot::channel::<()>();
            let fut = Fut {
                notified: notified.clone(),
                inner: rx,
            };
            (fut, tx, notified)
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

    #[tokio::test]
    async fn watch() {
        time::pause();

        let (signal, watch) = super::channel();

        // Setup a future to be drained. When draining begins, `drained0` is
        // flipped . When `tx0` fires, the whole `watch0` future completes.
        let (fut0, tx0, notified0) = Fut::new();
        let watch0 = watch
            .clone()
            .watch(fut0, |f| f.notified.store(true, SeqCst));
        tokio::pin!(watch0);

        // Setup another future to be drained.
        let (fut1, tx1, notified1) = Fut::new();
        let watch1 = watch.watch(fut1, |f| f.notified.store(true, SeqCst));
        tokio::pin!(watch1);

        // Ensure that none of the futures have completed and draining hasn't
        // been signaled.
        tokio::select! {
            _ = &mut watch0 => panic!("Future terminated early"),
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = futures::future::ready(()) => {}
        }
        assert!(!notified0.load(SeqCst));
        assert!(!notified1.load(SeqCst));

        // Signal draining and ensure that none of the futures have completed.
        let mut drain = tokio::spawn(signal.drain());

        tokio::select! {
            _ = &mut watch0 => panic!("Future terminated early"),
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = &mut drain => panic!("Drain terminated early"),
            _ = time::sleep(time::Duration::from_secs(1)) => {}
        }
        // Verify that the draining callbacks were invoked.
        assert!(notified0.load(SeqCst));
        assert!(notified1.load(SeqCst));

        // Complete the first watch.
        tx0.send(()).expect("must send");
        tokio::select! {
            _ = &mut watch0 => {},
            _ = &mut watch1 => panic!("Future terminated early"),
            _ = &mut drain => panic!("Drain terminated early"),
        }

        // Complete the second watch.
        tx1.send(()).expect("must send");

        // Ensure that all of our pending tasks, including the drain task,
        // complete.
        let done = async move {
            watch1.await;
            drain.await.expect("drain must succeed");
        };
        tokio::select! {
            _ = done => {}
            _ = time::sleep(time::Duration::from_secs(1)) => {
                panic!("Futures did not complete");
            }
        }
    }

    #[tokio::test]
    async fn drain() {
        let (signal, watch) = super::channel();

        let drain = tokio::spawn(signal.drain());

        let signaled = tokio::spawn(async move {
            let release = watch.signaled().await;
            drop(release);
        });

        // Ensure that all of our pending tasks, including the drain task,
        // complete.
        tokio::select! {
            res = signaled => res.expect("signaled must succeed"),
            _ = time::sleep(time::Duration::from_secs(1)) => {
                panic!("Signaled did not complete");
            }
        }
        tokio::select! {
            res = drain => res.expect("drain must succeed"),
            _ = time::sleep(time::Duration::from_secs(1)) => {
                panic!("Drain did not complete");
            }
        }
    }
}
