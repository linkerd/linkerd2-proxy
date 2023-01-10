use futures::{prelude::*, ready};
use indexmap::{map::Entry, IndexMap};
use linkerd_proxy_core::resolve::Update;
use pin_project::pin_project;
use std::{
    collections::VecDeque,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::discover::Change;
use tracing::{debug, trace};

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct FromResolve<T, R: TryStream> {
    #[pin]
    resolution: R,

    /// Changes that have been received but not yet emitted.
    pending: VecDeque<Change<SocketAddr, T>>,

    /// The current state of resolved endpoints that have been observed. This is
    /// an `IndexMap` so that the order of observed addresses is preserved
    /// (mostly for tests).
    active: IndexMap<SocketAddr, T>,
}

// === impl FromResolve ===

impl<T, R: TryStream> FromResolve<T, R> {
    pub fn new(resolution: R) -> Self {
        Self {
            resolution,
            active: IndexMap::default(),
            pending: VecDeque::new(),
        }
    }
}

impl<T, R> Stream for FromResolve<T, R>
where
    T: Clone + Eq + std::fmt::Debug,
    R: TryStream<Ok = Update<T>>,
{
    type Item = Result<Change<SocketAddr, T>, R::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            if let Some(change) = this.pending.pop_front() {
                debug!(?change, "Changed");
                return Poll::Ready(Some(Ok(change)));
            }

            trace!("poll");
            let update = match ready!(this.resolution.try_poll_next(cx)) {
                Some(update) => update?,
                None => return Poll::Ready(None),
            };

            match update {
                Update::Reset(endpoints) => {
                    let new_active = endpoints.into_iter().collect::<IndexMap<_, _>>();
                    trace!(new = ?new_active, old = ?this.active, "Reset");

                    for addr in this.active.keys() {
                        // If the old addr is not in the new set, remove it.
                        if !new_active.contains_key(addr) {
                            trace!(%addr, "Scheduling removal");
                            this.pending.push_back(Change::Remove(*addr));
                        } else {
                            trace!(%addr, "Unchanged");
                        }
                    }

                    for (addr, endpoint) in new_active.iter() {
                        if this.active.get(addr) != Some(endpoint) {
                            trace!(%addr, "Scheduling addition");
                            this.pending
                                .push_back(Change::Insert(*addr, endpoint.clone()));
                        }
                    }

                    *this.active = new_active;
                }

                Update::Add(endpoints) => {
                    for (addr, endpoint) in endpoints.into_iter() {
                        trace!(%addr, "Scheduling addition");
                        match this.active.entry(addr) {
                            Entry::Vacant(entry) => {
                                entry.insert(endpoint.clone());
                                this.pending.push_back(Change::Insert(addr, endpoint));
                            }
                            Entry::Occupied(mut entry) => {
                                if entry.get() != &endpoint {
                                    entry.insert(endpoint.clone());
                                    this.pending.push_back(Change::Insert(addr, endpoint));
                                }
                            }
                        }
                    }
                }

                Update::Remove(addrs) => {
                    for addr in addrs.into_iter() {
                        if this.active.remove(&addr).is_some() {
                            trace!(%addr, "Scheduling removal");
                            this.pending.push_back(Change::Remove(addr));
                        }
                    }
                }

                Update::DoesNotExist => {
                    trace!("Clearing all active endpoints");
                    this.pending
                        .extend(this.active.drain(..).map(|(sa, _)| Change::Remove(sa)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FromResolve;
    use futures::prelude::*;
    use linkerd_error::Infallible;
    use linkerd_proxy_core::resolve::Update;
    use std::net::SocketAddr;
    use tokio_stream::wrappers::ReceiverStream;
    use tower::discover::Change;

    const PORT: u16 = 8080;
    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 1, 1, n], PORT))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reset() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut disco = FromResolve::new(ReceiverStream::new(rx));

        // Use reset to set a new state with 3 addresses.
        tx.try_send(Ok::<_, Infallible>(Update::Reset(
            (1..=3).map(|n| (addr(n), n)).collect(),
        )))
        .expect("must send");
        for i in 1..=3 {
            assert!(matches!(
                disco.try_next().await,
                Ok(Some(Change::Insert(sa, n))) if sa == addr(i) && n == i
            ));
        }

        // Reset to a new state with 3 addresses, one of which is unchanged, one of which is
        // changed, and one of which is added.
        tx.try_send(Ok(Update::Reset(vec![
            // Restore the original `2` value. We shouldn't see an update for it.
            (addr(2), 2),
            // Set a new value for `3`.
            (addr(3), 4),
            // Add a new address, too.
            (addr(4), 5),
        ])))
        .expect("must send");
        // The first address is removed now.
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Remove(a))) if a == addr(1)
        ));
        // Then process the changed and new addresses.
        for i in 3..=4 {
            assert!(matches!(
                disco.try_next().await,
                Ok(Some(Change::Insert(sa, n))) if sa == addr(i) && n == i + 1
            ));
        }

        // No more updates.
        drop(tx);
        assert!(disco.next().await.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deduplicate_redundant() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut disco = FromResolve::new(ReceiverStream::new(rx));

        // The initial update is observed.
        tx.try_send(Ok::<_, Infallible>(Update::Add(vec![(addr(1), "a")])))
            .expect("must send");
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Insert(sa, "a"))) if sa == addr(1)
        ));

        // A redundant update is not.
        tx.try_send(Ok(Update::Add(vec![(addr(1), "a")])))
            .expect("must send");
        assert!(disco.try_next().now_or_never().is_none());

        // A new value for an existing address is observed.
        tx.try_send(Ok(Update::Add(vec![(addr(1), "b")])))
            .expect("must send");
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Insert(sa, "b"))) if sa == addr(1)
        ));

        // Remove the address.
        tx.try_send(Ok(Update::Remove(vec![addr(1)])))
            .expect("must send");
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Remove(sa))) if sa == addr(1)
        ));

        // Re-adding the address is observed.
        tx.try_send(Ok(Update::Add(vec![(addr(1), "b")])))
            .expect("must send");
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Insert(sa, "b"))) if sa == addr(1)
        ));

        // No more updates.
        drop(tx);
        assert!(disco.next().await.is_none(),);
    }
}
