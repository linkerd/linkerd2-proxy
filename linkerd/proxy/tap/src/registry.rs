use crate::grpc::TapTrace;
use crate::{iface, TapTraces};
use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::trace;

#[derive(Debug)]
pub struct Registry<T> {
    inner: Arc<Mutex<Inner<T>>>,
    taps_recv: watch::Receiver<Vec<T>>,
    trace_recv: TapTraces,
}

#[derive(Debug)]
struct Inner<T> {
    taps: Vec<T>,
    taps_send: watch::Sender<Vec<T>>,
    trace_send: watch::Sender<Option<TapTrace>>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        let (taps_send, taps_recv) = watch::channel(vec![]);
        let (trace_send, trace_recv) = watch::channel(None);
        let inner = Inner {
            taps: Vec::default(),
            taps_send,
            trace_send,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            taps_recv,
            trace_recv,
        }
    }
}

impl<T> Registry<T> {
    pub fn get_traces(&self) -> TapTraces {
        self.trace_recv.clone()
    }
}

impl<T> Registry<T>
where
    T: iface::Tap,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_taps(&self) -> Vec<T> {
        self.taps_recv.borrow().clone()
    }

    pub fn set_trace(&self, tr: TapTrace) {
        let inner = self.inner.lock();
        let _ = inner.trace_send.send(Some(tr));
    }

    pub fn clear_trace(&self) {
        let inner = self.inner.lock();
        let _ = inner.trace_send.send(None);
    }

    pub fn register(&self, tap: T) {
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

impl<T> Clone for Registry<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            taps_recv: self.taps_recv.clone(),
            trace_recv: self.trace_recv.clone(),
        }
    }
}
