use futures::{prelude::*, ready};
use linkerd_proxy_core::resolve::{Resolve, Update};
use pin_project::pin_project;
use std::{
    collections::{btree_map, BTreeMap, VecDeque},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::discover::Change;
use tracing::{debug, trace};

#[derive(Clone, Debug)]
pub struct FromResolve<R, E> {
    resolve: R,
    _marker: std::marker::PhantomData<fn() -> E>,
}

#[pin_project]
#[derive(Debug)]
pub struct DiscoverFuture<F, E> {
    #[pin]
    future: F,
    _marker: std::marker::PhantomData<fn() -> E>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct Discover<R: TryStream, E> {
    #[pin]
    resolution: R,
    active: BTreeMap<SocketAddr, E>,
    pending: VecDeque<Change<SocketAddr, E>>,
}

// === impl FromResolve ===

impl<R, E> FromResolve<R, E> {
    pub fn new(resolve: R) -> Self {
        Self {
            resolve,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, R, E> tower::Service<T> for FromResolve<R, E>
where
    R: Resolve<T> + Clone,
{
    type Response = Discover<R::Resolution, E>;
    type Error = R::Error;
    type Future = DiscoverFuture<R::Future, E>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resolve.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        Self::Future {
            future: self.resolve.resolve(target),
            _marker: std::marker::PhantomData,
        }
    }
}

// === impl DiscoverFuture ===

impl<F, E> Future for DiscoverFuture<F, E>
where
    F: TryFuture,
    F::Ok: TryStream,
{
    type Output = Result<Discover<F::Ok, E>, F::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let resolution = ready!(self.project().future.try_poll(cx))?;
        Poll::Ready(Ok(Discover::new(resolution)))
    }
}

// === impl Discover ===

impl<R: TryStream, E> Discover<R, E> {
    pub fn new(resolution: R) -> Self {
        Self {
            resolution,
            active: BTreeMap::default(),
            pending: VecDeque::new(),
        }
    }
}

impl<R, E> Stream for Discover<R, E>
where
    R: TryStream<Ok = Update<E>>,
    E: Clone + Eq + std::fmt::Debug,
{
    type Item = Result<Change<SocketAddr, E>, R::Error>;

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
                    let new_active = endpoints.into_iter().collect::<BTreeMap<_, _>>();
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
                            btree_map::Entry::Vacant(entry) => {
                                entry.insert(endpoint.clone());
                                this.pending.push_back(Change::Insert(addr, endpoint));
                            }
                            btree_map::Entry::Occupied(mut entry) => {
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
                    trace!("Scheduling removals");
                    this.pending
                        .extend(this.active.keys().copied().map(Change::Remove));
                    this.active.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Discover;
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
        let mut disco = Discover::new(ReceiverStream::new(rx));

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
        tx.try_send(Ok(Update::Reset(
            // Restore the original `2` value. We shouldn't see an update for it.
            Some((addr(2), 2))
                .into_iter()
                // Set a new value for `3`. and add a 4th as well.
                .chain((3..=4).map(|n| (addr(n), n + 1)))
                .collect(),
        )))
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
    async fn dedupe() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut disco = Discover::new(ReceiverStream::new(rx));

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
        tokio::select! {
            biased;
            _ = disco.try_next() => panic!("must not receive"),
            _ = futures::future::ready(()) => {}
        }

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
        tx.try_send(Ok(Update::Add(vec![(addr(1), "c")])))
            .expect("must send");
        assert!(matches!(
            disco.try_next().await,
            Ok(Some(Change::Insert(sa, "c"))) if sa == addr(1)
        ));

        // No more updates.
        drop(tx);
        assert!(disco.next().await.is_none(),);
    }
}
