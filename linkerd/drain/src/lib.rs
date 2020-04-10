#![deny(warnings, rust_2018_idioms)]

use futures::{future::Shared, FutureExt, TryFutureExt};
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
    let (drained_tx, drained_rx) = mpsc::channel(0);
    (
        Signal { drained_rx, tx },
        Watch {
            drained_tx,
            rx: rx.map_err((|_| ()) as fn(_) -> _).shared(),
        },
    )
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
        futures::future::MapErr<
            oneshot::Receiver<()>,
            fn(tokio::sync::oneshot::error::RecvError) -> (),
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
            _ = self.rx => {
                on_drain(&mut future);
                (&mut future).await
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        Arc,
    };
    use tokio_test::{assert_pending, assert_ready, task};
    struct TestMe {
        draining: AtomicBool,
        finished: AtomicBool,
        poll_cnt: AtomicUsize,
    }

    struct TestMeFut(Arc<TestMe>);

    impl Future for TestMeFut {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.as_ref();
            this.0.poll_cnt.fetch_add(1, Relaxed);
            if this.0.finished.load(Relaxed) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    #[test]
    fn watch() {
        let (tx, rx) = channel();
        let fut = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });

        let mut watch = task::spawn(rx.watch(TestMeFut(fut.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        assert_eq!(fut.poll_cnt.load(Relaxed), 0);

        // First poll should poll the inner future, 1);
        assert_pending!(watch.poll());
        assert_eq!(fut.poll_cnt.load(Relaxed), 1);

        // Second poll should poll the inner future again
        assert_pending!(watch.poll());
        assert_eq!(fut.poll_cnt.load(Relaxed), 2);

        let mut draining = task::spawn(tx.drain());
        // Drain signaled, but needs another poll to be noticed.
        assert!(!fut.draining.load(Relaxed));
        assert_eq!(fut.poll_cnt.load(Relaxed), 2);

        // Now, poll after drain has been signaled.
        assert_pending!(watch.poll());
        assert!(fut.draining.load(Relaxed));
        assert_eq!(fut.poll_cnt.load(Relaxed), 3);

        // Draining is not ready until watcher completes
        assert_pending!(draining.poll());

        // Finishing up the watch future
        fut.finished.store(true, Relaxed);
        assert_ready!(watch.poll());
        assert_eq!(fut.poll_cnt.load(Relaxed), 4);
        drop(watch);

        assert_ready!(draining.poll());
    }

    #[test]
    fn watch_clones() {
        let (tx, rx) = channel();
        let fut1 = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });
        let fut2 = Arc::new(TestMe {
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            poll_cnt: AtomicUsize::new(0),
        });

        let watch1 = task::spawn(rx.clone().watch(TestMeFut(fut1.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        let watch2 = task::spawn(rx.clone().watch(TestMeFut(fut2.clone()), |fut| {
            fut.0.draining.store(true, Relaxed)
        }));

        let mut draining = task::spawn(tx.drain());

        // Still 2 outstanding watchers
        assert_pending!(draining.poll());

        // drop 1 for whatever reason
        drop(watch1);

        // Still not ready, 1 other watcher still pending
        assert_pending!(draining.poll());

        drop(watch2);

        // Now all watchers are gone, draining is complete
        assert_ready!(draining.poll())
    }
}
