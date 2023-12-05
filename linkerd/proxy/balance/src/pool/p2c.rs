//! A pool that uses the power-of-two-choices algorithm to select endpoints.
//!
// Based on tower::p2c::Balance. Copyright (c) 2019 Tower Contributors

use super::{Pool, Update};
use ahash::AHashMap;
use futures_util::TryFutureExt;
use linkerd_error::Error;
use linkerd_stack::{NewService, Service};
use rand::{rngs::SmallRng, thread_rng, Rng, SeedableRng};
use std::{
    collections::hash_map::Entry,
    net::SocketAddr,
    task::{Context, Poll},
};
use tower::{
    load::Load,
    ready_cache::{error::Failed, ReadyCache},
};

/// Dispatches requests to a pool of services selected by the
/// power-of-two-choices algorithm.
#[derive(Debug)]
pub struct P2cPool<T, N, Req, S> {
    new_endpoint: N,
    endpoints: AHashMap<SocketAddr, T>,
    pool: ReadyCache<SocketAddr, S, Req>,
    rng: SmallRng,
    next_idx: Option<usize>,
}

impl<T, N, Req, S> P2cPool<T, N, Req, S>
where
    T: Clone + Eq,
    N: NewService<(SocketAddr, T), Service = S>,
    S: Service<Req> + Load,
    S::Error: Into<Error>,
    S::Metric: std::fmt::Debug,
{
    pub fn new(new_endpoint: N) -> Self {
        let rng = SmallRng::from_rng(&mut thread_rng()).expect("RNG must be seeded");
        Self {
            rng,
            new_endpoint,
            next_idx: None,
            pool: ReadyCache::default(),
            endpoints: Default::default(),
        }
    }

    /// Resets the pool to include the given targets without unnecessarily
    /// rebuilding inner services.
    ///
    /// Returns true if the pool was changed.
    fn reset(&mut self, targets: Vec<(SocketAddr, T)>) -> bool {
        let mut changed = false;
        let mut remaining = std::mem::take(&mut self.endpoints);
        for (addr, target) in targets.into_iter() {
            remaining.remove(&addr);
            match self.endpoints.entry(addr) {
                Entry::Occupied(mut e) => {
                    if e.get() == &target {
                        continue;
                    }
                    e.insert(target.clone());
                }
                Entry::Vacant(e) => {
                    e.insert(target.clone());
                }
            }
            let svc = self.new_endpoint.new_service((addr, target));
            self.pool.push(addr, svc);
            changed = true;
        }
        for (addr, _) in remaining.drain() {
            changed = self.pool.evict(&addr) || changed;
        }
        changed
    }

    /// Adds endpoints to the pool without unnecessarily rebuilding inner
    /// services.
    ///
    /// Returns true if the pool was changed.
    fn add(&mut self, targets: Vec<(SocketAddr, T)>) -> bool {
        let mut changed = false;
        for (addr, target) in targets.into_iter() {
            match self.endpoints.entry(addr) {
                Entry::Occupied(mut e) => {
                    if e.get() == &target {
                        continue;
                    }
                    e.insert(target.clone());
                }
                Entry::Vacant(e) => {
                    e.insert(target.clone());
                }
            }
            let svc = self.new_endpoint.new_service((addr, target));
            self.pool.push(addr, svc);
            changed = true;
        }
        changed
    }

    /// Removes endpoint services.
    ///
    /// Returns true if the pool was changed.
    fn remove(&mut self, addrs: Vec<SocketAddr>) -> bool {
        let mut changed = false;
        for addr in addrs.into_iter() {
            if self.endpoints.remove(&addr).is_some() {
                changed = self.pool.evict(&addr) || changed;
            }
        }
        changed
    }

    /// Clear all endpoints from the pool.
    ///
    /// Returns true if the pool was changed.
    fn clear(&mut self) -> bool {
        let mut changed = false;
        for (addr, _) in self.endpoints.drain() {
            changed = self.pool.evict(&addr) || changed;
        }
        changed
    }

    fn p2c_ready_index(&mut self) -> Option<usize> {
        match self.pool.ready_len() {
            0 => None,
            1 => Some(0),
            len => {
                // Get two distinct random indexes (in a random order) and
                // compare the loads of the service at each index.
                let aidx = self.rng.gen_range(0..len);
                let mut bidx = self.rng.gen_range(0..(len - 1));
                if bidx >= aidx {
                    bidx += 1;
                }
                debug_assert_ne!(aidx, bidx, "random indices must be distinct");

                let aload = self.ready_index_load(aidx);
                let bload = self.ready_index_load(bidx);
                let chosen = if aload <= bload { aidx } else { bidx };

                tracing::trace!(
                    a.index = aidx,
                    a.load = ?aload,
                    b.index = bidx,
                    b.load = ?bload,
                    chosen = if chosen == aidx { "a" } else { "b" },
                    "p2c",
                );
                Some(chosen)
            }
        }
    }

    /// Accesses a ready endpoint by index and returns its current load.
    fn ready_index_load(&self, index: usize) -> S::Metric {
        let (_, svc) = self.pool.get_ready_index(index).expect("invalid index");
        svc.load()
    }
}

impl<T, N, Req, S> Pool<T, Req> for P2cPool<T, N, Req, S>
where
    T: Clone + Eq + std::fmt::Debug,
    N: NewService<(SocketAddr, T), Service = S>,
    S: Service<Req> + Load,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
    S::Metric: std::fmt::Debug,
{
    fn update_pool(&mut self, update: Update<T>) {
        tracing::trace!(?update);
        let changed = match update {
            Update::Reset(targets) => self.reset(targets),
            Update::Add(targets) => self.add(targets),
            Update::Remove(addrs) => self.remove(addrs),
            Update::DoesNotExist => self.clear(),
        };
        if changed {
            self.next_idx = None;
        }
    }

    /// Moves pending endpoints to ready.
    ///
    /// This must be called from the same task that invokes Service::poll_ready.
    fn poll_pool(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        tracing::trace!("Polling pending");
        self.pool.poll_pending(cx).map_err(|Failed(_, e)| e)
    }
}

impl<T, N, Req, S> Service<Req> for P2cPool<T, N, Req, S>
where
    T: Clone + Eq,
    N: NewService<(SocketAddr, T), Service = S>,
    S: Service<Req> + Load,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
    S::Metric: std::fmt::Debug,
{
    type Response = S::Response;
    type Error = Error;
    type Future = futures::future::ErrInto<S::Future, Error>;

    /// Returns ready when at least one endpoint is ready.
    ///
    /// If multiple endpoints are ready, the power-of-two-choices algorithm is
    /// used to select one.
    ///
    /// NOTE that this may return `Pending` when there are no endpoints. In such
    /// cases, the caller must invoke `update_pool` and then wait for new
    /// endpoints to become ready.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            tracing::trace!(pending = self.pool.pending_len(), "Polling pending");
            match self.pool.poll_pending(cx)? {
                Poll::Ready(()) => tracing::trace!("All endpoints are ready"),
                Poll::Pending => tracing::trace!("Endpoints are pending"),
            }

            let idx = match self.next_idx.take().or_else(|| self.p2c_ready_index()) {
                Some(idx) => idx,
                None => {
                    tracing::debug!("No ready endpoints");
                    return Poll::Pending;
                }
            };

            tracing::trace!(ready.index = idx, "Selected");
            if !self.pool.check_ready_index(cx, idx)? {
                tracing::trace!(ready.index = idx, "Reverted to pending");
                continue;
            }

            tracing::trace!(ready.index = idx, "Ready");
            self.next_idx = Some(idx);
            return Poll::Ready(Ok(()));
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self.next_idx.take().expect("call before ready");
        self.pool.call_ready_index(idx, req).err_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::prelude::*;
    use linkerd_stack::ServiceExt;
    use tokio::time;
    use tower::load::{CompleteOnResponse, PeakEwma};

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn update_pool() {
        let addr0 = "192.168.10.10:80".parse().unwrap();
        let addr1 = "192.168.10.11:80".parse().unwrap();

        let mut pool = P2cPool::new(|(_addr, _n)| {
            PeakEwma::new(
                linkerd_stack::service_fn(|()| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(()))
                }),
                time::Duration::from_secs(1),
                1.0 * 1000.0 * 1000.0,
                CompleteOnResponse::default(),
            )
        });

        pool.update_pool(Update::Reset(vec![(addr0, 0)]));
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(pool.endpoints.get(&addr0), Some(&0));

        pool.update_pool(Update::Add(vec![(addr0, 1)]));
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(pool.endpoints.get(&addr0), Some(&1));

        pool.update_pool(Update::Reset(vec![(addr0, 1)]));
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(pool.endpoints.get(&addr0), Some(&1));

        pool.update_pool(Update::Add(vec![(addr1, 1)]));
        assert_eq!(pool.endpoints.len(), 2);
        assert_eq!(pool.endpoints.get(&addr1), Some(&1));

        pool.update_pool(Update::Remove(vec![addr0]));
        assert_eq!(pool.endpoints.len(), 1);

        pool.update_pool(Update::Reset(vec![(addr0, 2), (addr1, 2)]));
        assert_eq!(pool.endpoints.len(), 2);
        assert_eq!(pool.endpoints.get(&addr0), Some(&2));
        assert_eq!(pool.endpoints.get(&addr1), Some(&2));

        pool.update_pool(Update::DoesNotExist);
        assert_eq!(pool.endpoints.len(), 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn p2c_ready_index() {
        let addr0 = "192.168.10.10:80".parse().unwrap();
        let (svc0, mut h0) = tower_test::mock::pair::<(), ()>();
        h0.allow(0);

        let addr1 = "192.168.10.11:80".parse().unwrap();
        let (svc1, mut h1) = tower_test::mock::pair::<(), ()>();
        h1.allow(0);

        let addr2 = "192.168.10.12:80".parse().unwrap();
        let (svc2, mut h2) = tower_test::mock::pair::<(), ()>();
        h2.allow(0);

        let mut pool = P2cPool::new(|(a, ())| {
            PeakEwma::new(
                if a == addr0 {
                    svc0.clone()
                } else if a == addr1 {
                    svc1.clone()
                } else if a == addr2 {
                    svc2.clone()
                } else {
                    panic!("unexpected address: {a}");
                },
                time::Duration::from_secs(1),
                1.0 * 1000.0 * 1000.0,
                CompleteOnResponse::default(),
            )
        });

        pool.update_pool(Update::Reset(vec![(addr0, ())]));
        assert!(pool.ready().now_or_never().is_none());
        assert!(pool.next_idx.is_none());

        h0.allow(1);
        assert!(pool.ready().now_or_never().is_some());
        assert_eq!(pool.next_idx, Some(0));

        h1.allow(1);
        h2.allow(1);
        pool.update_pool(Update::Reset(vec![(addr0, ()), (addr1, ()), (addr2, ())]));
        assert!(pool.next_idx.is_none());
        assert!(pool.ready().now_or_never().is_some());
        assert!(pool.next_idx.is_some());

        let call = pool.call(());
        let ((), respond) = tokio::select! {
            r = h0.next_request() => r.unwrap(),
            r = h1.next_request() => r.unwrap(),
            r = h2.next_request() => r.unwrap(),
        };
        respond.send_response(());
        call.now_or_never()
            .expect("call should be satisfied")
            .expect("call should succeed");
    }
}
