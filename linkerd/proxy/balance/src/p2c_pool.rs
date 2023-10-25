use futures_util::TryFutureExt;
use linkerd_error::Error;
use linkerd_proxy_pool::{Pool, Update};
use linkerd_stack::{ready_cache::ReadyCache, NewService, Service};
use rand::{rngs::SmallRng, thread_rng, Rng, SeedableRng};
use rustc_hash::FxHashMap;
use std::{
    collections::hash_map::Entry,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{
    load::{self, Load},
    ready_cache::error::Failed,
};

#[derive(Debug)]
pub struct P2cPool<T, N, Req, S> {
    new_endpoint: N,
    endpoints: FxHashMap<SocketAddr, T>,
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
        Self {
            new_endpoint,
            endpoints: FxHashMap::default(),
            pool: ReadyCache::default(),
            rng: SmallRng::from_rng(&mut thread_rng()).expect("RNG must be seeded"),
            next_idx: None,
        }
    }

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
            self.pool.evict(&addr);
            changed = true;
        }
        changed
    }

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

    fn remove(&mut self, addrs: Vec<SocketAddr>) -> bool {
        let mut changed = false;
        for addr in addrs.into_iter() {
            if self.endpoints.remove(&addr).is_some() {
                self.pool.evict(&addr);
                changed = true;
            }
        }
        changed
    }

    fn clear(&mut self) -> bool {
        let mut changed = false;
        for (addr, _) in self.endpoints.drain() {
            self.pool.evict(&addr);
            changed = true;
        }
        changed
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Result<(), Error> {
        if let Poll::Ready(Err(e)) = self.pool.poll_pending(cx) {
            return Err(e.into());
        }
        Ok(())
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
                    bidx = bidx + 1;
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            tracing::trace!("Polling pending");
            if let Err(e) = self.poll_pending(cx) {
                return Poll::Ready(Err(e));
            }

            if let Some(idx) = self.next_idx {
                tracing::trace!(ready.index = idx, "Check");
                if self.pool.check_ready_index(cx, idx)? {
                    tracing::trace!(ready.index = idx, "Ready");
                    return Poll::Ready(Ok(()));
                }
                tracing::trace!(ready.index = idx, "Pending");
                self.next_idx = None;
            }

            self.next_idx = self.p2c_ready_index();
            if self.next_idx.is_none() {
                return Poll::Pending;
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self.next_idx.take().expect("call before ready");
        self.pool.call_ready_index(idx, req).err_into()
    }
}
