use super::iface::Tap;
use futures::{ready, Stream};
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

pub fn new<T>() -> (Daemon<T>, Register<T>, Subscribe<T>) {
    let (svc_tx, svc_rx) = mpsc::channel(super::REGISTER_CHANNEL_CAPACITY);
    let (tap_tx, tap_rx) = mpsc::channel(super::TAP_CAPACITY);

    let daemon = Daemon {
        svc_rx,
        svcs: Vec::default(),

        tap_rx,
        taps: Vec::default(),
    };

    (daemon, Register(svc_tx), Subscribe(tap_tx))
}

/// A background task that connects a tap server and proxy services.
///
/// The daemon provides `Register` to allow proxy services to listen for new
/// taps; and it provides `Subscribe` to allow the tap server to advertise new
/// taps to proxy services.
#[pin_project]
#[must_use = "daemon must be polled"]
#[derive(Debug)]
pub struct Daemon<T> {
    #[pin]
    svc_rx: mpsc::Receiver<mpsc::Sender<T>>,
    svcs: Vec<mpsc::Sender<T>>,

    #[pin]
    tap_rx: mpsc::Receiver<(T, oneshot::Sender<()>)>,
    taps: Vec<T>,
}

#[derive(Debug)]
pub struct Register<T>(mpsc::Sender<mpsc::Sender<T>>);

#[derive(Debug)]
pub struct Subscribe<T>(mpsc::Sender<(T, oneshot::Sender<()>)>);

#[pin_project]
#[derive(Debug)]
pub struct SubscribeFuture<T>(#[pin] FutState<T>);

#[pin_project]
#[derive(Debug)]
enum FutState<T> {
    Subscribe {
        tap: Option<T>,
        tap_tx: mpsc::Sender<(T, oneshot::Sender<()>)>,
    },
    Pending(#[pin] oneshot::Receiver<()>),
}

impl<T: Tap> Future for Daemon<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        // Drop taps that are no longer active (i.e. the response stream has
        // been dropped).
        let tap_count = this.taps.len();
        this.taps.retain(|t| t.can_tap_more());
        trace!("retained {} of {} taps", this.taps.len(), tap_count);

        // Drop services that are no longer active.
        for idx in (0..this.svcs.len()).rev() {
            // It's "okay" if a service isn't ready to receive taps. We just
            // fall back to being lossy rather than dropping the service
            // entirely.
            if let Poll::Ready(Err(_)) = this.svcs[idx].poll_ready(cx) {
                trace!("removing a service");
                this.svcs.swap_remove(idx);
            }
        }

        // Connect newly-created services to active taps.
        while let Poll::Ready(Some(mut svc)) = this.svc_rx.as_mut().poll_next(cx) {
            trace!("registering a service");

            // Notify the service of all active taps.
            let mut dropped = false;
            for tap in &*this.taps {
                debug_assert!(!dropped);

                let err = svc.try_send(tap.clone()).err();

                // If the service has been dropped, make sure that it's not added.
                dropped = err
                    .as_ref()
                    .map(|e| match e {
                        tokio::sync::mpsc::error::TrySendError::Closed(_) => true,
                        _ => false,
                    })
                    .unwrap_or(false);

                // If service can't receive any more taps, stop trying.
                if err.is_some() {
                    break;
                }
            }

            if !dropped {
                this.svcs.push(svc);
                trace!("service registered");
            }
        }

        // Connect newly-created taps to existing services.
        while let Poll::Ready(Some((tap, ack))) = this.tap_rx.as_mut().poll_next(cx) {
            trace!("subscribing a tap");
            if this.taps.len() == super::TAP_CAPACITY {
                warn!("tap capacity exceeded");
                drop(ack);
                continue;
            }
            if !tap.can_tap_more() {
                trace!("tap already dropped");
                drop(ack);
                continue;
            }

            // Notify services of the new tap. If the service has been dropped,
            // it's removed from the registry. If it's full, it isn't notified
            // of the tap.
            for idx in (0..this.svcs.len()).rev() {
                let err = this.svcs[idx].try_send(tap.clone()).err();
                if err
                    .map(|e| match e {
                        tokio::sync::mpsc::error::TrySendError::Closed(_) => true,
                        _ => false,
                    })
                    .unwrap_or(false)
                {
                    trace!("removing a service");
                    this.svcs.swap_remove(idx);
                }
            }

            this.taps.push(tap);
            let _ = ack.send(());
            trace!("tap subscribed");
        }

        Poll::Pending
    }
}

impl<T: Tap> Clone for Register<T> {
    fn clone(&self) -> Self {
        Register(self.0.clone())
    }
}

impl<T: Tap> super::iface::Register for Register<T> {
    type Tap = T;
    type Taps = mpsc::Receiver<T>;

    fn register(&mut self) -> Self::Taps {
        let (tx, rx) = mpsc::channel(super::TAP_CAPACITY);
        if let Err(_) = self.0.try_send(tx) {
            debug!("failed to register service");
        }
        rx
    }
}

impl<T: Tap> Clone for Subscribe<T> {
    fn clone(&self) -> Self {
        Subscribe(self.0.clone())
    }
}

impl<T: Tap> super::iface::Subscribe<T> for Subscribe<T> {
    type Future = SubscribeFuture<T>;

    fn subscribe(&self, tap: T) -> Self::Future {
        SubscribeFuture(FutState::Subscribe {
            tap: Some(tap),
            tap_tx: self.0.clone(),
        })
    }
}

impl<T: Tap> Future for SubscribeFuture<T> {
    type Output = Result<(), super::iface::NoCapacity>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.0.as_mut().project() {
                FutState::Subscribe { tap, tap_tx } => {
                    ready!(tap_tx.poll_ready(cx)).map_err(|_| super::iface::NoCapacity)?;

                    let tap = tap.take().expect("tap must be set");
                    let (tx, rx) = oneshot::channel();
                    tap_tx
                        .try_send((tap, tx))
                        .map_err(|_| super::iface::NoCapacity)?;

                    this.0.as_mut().set(FutState::Pending(rx))
                }
                FutState::Pending(rx) => {
                    return Poll::Ready(ready!(rx.poll(cx)).map_err(|_| super::iface::NoCapacity));
                }
            }
        }
    }
}
