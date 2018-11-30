use futures::sync::{mpsc, oneshot};
use futures::{Async, Future, Poll, Stream};
use never::Never;

use super::iface::Tap;

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
#[must_use = "daemon must be polled"]
#[derive(Debug)]
pub struct Daemon<T> {
    svc_rx: mpsc::Receiver<mpsc::Sender<T>>,
    svcs: Vec<mpsc::Sender<T>>,

    tap_rx: mpsc::Receiver<(T, oneshot::Sender<()>)>,
    taps: Vec<T>,
}

#[derive(Debug)]
pub struct Register<T>(mpsc::Sender<mpsc::Sender<T>>);

#[derive(Debug)]
pub struct Subscribe<T>(mpsc::Sender<(T, oneshot::Sender<()>)>);

#[derive(Debug)]
pub struct SubscribeFuture<T>(FutState<T>);

#[derive(Debug)]
enum FutState<T> {
    Subscribe {
        tap: Option<T>,
        tap_tx: mpsc::Sender<(T, oneshot::Sender<()>)>,
    },
    Pending(oneshot::Receiver<()>),
}

impl<T: Tap> Future for Daemon<T> {
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<(), Never> {
        // Drop taps that are no longer active (i.e. the response stream has
        // been dropped).
        let tap_count = self.taps.len();
        self.taps.retain(|t| t.can_tap_more());
        trace!("retained {} of {} taps", self.taps.len(), tap_count);

        // Drop services that are no longer active.
        for idx in (0..self.svcs.len()).rev() {
            // It's "okay" if a service isn't ready to receive taps. We just
            // fall back to being lossy rather than dropping the service
            // entirely.
            if self.svcs[idx].poll_ready().is_err() {
                trace!("removing a service");
                self.svcs.swap_remove(idx);
            }
        }

        // Connect newly-created services to active taps.
        while let Ok(Async::Ready(Some(mut svc))) = self.svc_rx.poll() {
            trace!("registering a service");

            // Notify the service of all active taps.
            let mut dropped = false;
            for tap in &self.taps {
                debug_assert!(!dropped);

                let err = svc.try_send(tap.clone()).err();

                // If the service has been dropped, make sure that it's not added.
                dropped = err.as_ref().map(|e| e.is_disconnected()).unwrap_or(false);

                // If service can't receive any more taps, stop trying.
                if err.is_some() {
                    break;
                }
            }

            if !dropped {
                self.svcs.push(svc);
                trace!("service registered");
            }
        }

        // Connect newly-created taps to existing services.
        while let Ok(Async::Ready(Some((tap, ack)))) = self.tap_rx.poll() {
            trace!("subscribing a tap");
            if self.taps.len() == super::TAP_CAPACITY {
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
            for idx in (0..self.svcs.len()).rev() {
                let err = self.svcs[idx].try_send(tap.clone()).err();
                if err.map(|e| e.is_disconnected()).unwrap_or(false) {
                    trace!("removing a service");
                    self.svcs.swap_remove(idx);
                }
            }

            self.taps.push(tap);
            let _ = ack.send(());
            trace!("tap subscribed");
        }

        Ok(Async::NotReady)
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

    fn subscribe(&mut self, tap: T) -> Self::Future {
        SubscribeFuture(FutState::Subscribe {
            tap: Some(tap),
            tap_tx: self.0.clone(),
        })
    }
}

impl<T: Tap> Future for SubscribeFuture<T> {
    type Item = ();
    type Error = super::iface::NoCapacity;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        loop {
            self.0 = match self.0 {
                FutState::Subscribe {
                    ref mut tap,
                    ref mut tap_tx,
                } => {
                    try_ready!(tap_tx.poll_ready().map_err(|_| super::iface::NoCapacity));

                    let tap = tap.take().expect("tap must be set");
                    let (tx, rx) = oneshot::channel();
                    tap_tx
                        .try_send((tap, tx))
                        .map_err(|_| super::iface::NoCapacity)?;

                    FutState::Pending(rx)
                }
                FutState::Pending(ref mut rx) => {
                    return rx.poll().map_err(|_| super::iface::NoCapacity);
                }
            }
        }
    }
}
