use crate::grpc::Tap;
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::trace;

/// A registry containing all the active taps that have registered with the
/// gRPC server.
#[derive(Debug)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
    taps_recv: watch::Receiver<Vec<Tap>>,
}

#[derive(Debug)]
struct Inner {
    taps: Vec<Tap>,
    taps_send: watch::Sender<Vec<Tap>>,
}

impl Default for Registry {
    fn default() -> Self {
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
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_taps(&self) -> Vec<Tap> {
        self.taps_recv.borrow().clone()
    }

    pub fn register(&self, tap: Tap) {
        let mut inner = self.inner.lock();
        inner.taps.push(tap);
        let _ = inner.taps_send.send(inner.taps.clone());
    }

    pub async fn clean(self, wakeup: impl Stream) {
        futures::pin_mut!(wakeup);
        while wakeup.next().await.is_some() {
            let mut inner = self.inner.lock();
            let count = inner.taps.len();
            inner.taps.retain(|tap| tap.can_tap_more());
            trace!("retained {} of {} taps", inner.taps.len(), count);
        }
    }
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            taps_recv: self.taps_recv.clone(),
        }
    }
}
