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
#[must_use = "handle must be held until purge should complete"]
pub struct Handle(mpsc::Sender<Never>);

// ===== impl Purge =====

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

#[cfg(test)]
pub mod tests {
    use super::*;
    use futures::future;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const UNUSED: Duration = Duration::from_secs(12345);

    #[test]
    fn completes_on_handle_drop() {
        tokio::run(future::lazy(|| {
            let (purge, purge_handle) = Purge::new(Lock::new(Cache::<usize, ()>::new(2, UNUSED)));

            let polls = Arc::new(());
            let polls_handle = Arc::downgrade(&polls);
            struct Wrap(Option<Arc<()>>, Purge<usize, ()>);
            impl Future for Wrap {
                type Item = ();
                type Error = ();
                fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
                    match self.1.poll() {
                        Err(n) => match n {},
                        Ok(Async::Ready(())) => {
                            drop(self.0.take().unwrap());
                            Ok(Async::Ready(()))
                        }
                        Ok(Async::NotReady) => Ok(Async::NotReady),
                    }
                }
            }
            tokio::spawn(Wrap(Some(polls), purge));

            drop(purge_handle);

            tokio::timer::Delay::new(Instant::now() + Duration::from_millis(100)).then(move |_| {
                assert!(polls_handle.upgrade().is_none());
                Ok(())
            })
        }));
    }
}
