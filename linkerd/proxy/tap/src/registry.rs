use crate::iface;
use futures::{Stream, StreamExt};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct Registry<T> {
    inner: Arc<Mutex<Inner<T>>>,
    taps_recv: watch::Receiver<Vec<T>>,
}

#[derive(Debug)]
struct Inner<T> {
    taps: Vec<T>,
    taps_send: watch::Sender<Vec<T>>,
}

impl<T> Registry<T>
where
    T: iface::Tap + Clone,
{
    pub fn new() -> Self {
        let (taps_send, taps_recv) = watch::channel(vec![]);
        let inner = Inner {
            taps: Vec::default(),
            taps_send,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            taps_recv,
        }
    }

    pub fn get_taps(&self) -> Vec<T> {
        self.taps_recv.borrow().clone()
    }

    pub fn register(&self, tap: T) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.taps.push(tap);
            let _ = inner.taps_send.broadcast(inner.taps.clone());
        }
    }

    pub async fn clean(self, wakeup: impl Stream) {
        futures::pin_mut!(wakeup);
        while let Some(_) = wakeup.next().await {
            if let Ok(mut inner) = self.inner.lock() {
                let count = inner.taps.len();
                inner.taps.retain(|tap| tap.can_tap_more());
                trace!("retained {} of {} taps", inner.taps.len(), count);
            }
        }
    }
}
