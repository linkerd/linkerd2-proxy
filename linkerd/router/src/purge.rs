use super::Cache;
use futures::{Async, Future, Poll, Stream};
use linkerd2_error::Never;
use std::hash::Hash;
use tokio::sync::lock::Lock;
use tokio::sync::mpsc;

/// A background future that eagerly removes expired cache values.
///
/// If the cache is dropped, this future will complete.
pub struct Purge<K: Clone + Eq + Hash, V> {
    cache: Lock<Cache<K, V>>,
    hangup: mpsc::Receiver<Never>,
}

/// Ensures that `Purge` runs until all handles are dropped.
#[derive(Clone)]
pub struct Handle(mpsc::Sender<Never>);

// ===== impl PurgeCache =====

impl<K, V> Purge<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(cache: Lock<Cache<K, V>>) -> (Self, Handle) {
        let (tx, hangup) = mpsc::channel(1);
        (Purge { cache, hangup }, Handle(tx))
    }
}

impl<K, V> Future for Purge<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    type Item = ();
    type Error = Never;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.hangup.poll() {
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
            Ok(Async::Ready(Some(never))) => match never {},
            Err(_) => unreachable!("purge hangup handle must not error"),
        };

        if let Async::Ready(mut cache) = self.cache.poll_lock() {
            cache.purge();
        }

        Ok(Async::NotReady)
    }
}
