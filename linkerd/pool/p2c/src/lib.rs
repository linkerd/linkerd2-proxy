//! A pool that uses the power-of-two-choices algorithm to select endpoints.
//!
// Based on tower::p2c::Balance. Copyright (c) 2019 Tower Contributors

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use ahash::AHashMap;
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_metrics::prom;
use linkerd_pool::Pool;
use linkerd_stack::{NewService, Service};
use rand::{rngs::SmallRng, Rng, SeedableRng};
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
    metrics: P2cMetrics,
    next_idx: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct P2cMetricFamilies<L> {
    endpoints: prom::Family<L, prom::Gauge>,
    updates: prom::Family<UpdateLabels<L>, prom::Counter>,
}

#[derive(Clone, Debug, Default)]
pub struct P2cMetrics {
    endpoints: prom::Gauge,

    /// Measures the number of Reset updates received from service discovery.
    updates_reset: prom::Counter,

    /// Measures the number of Add updates received from service discovery.
    updates_add: prom::Counter,

    /// Measures the number of Remove updates received from service discovery.
    updates_rm: prom::Counter,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
struct UpdateLabels<L> {
    op: UpdateOp,
    labels: L,
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, prom::encoding::EncodeLabelValue)]
enum UpdateOp {
    Reset,
    Add,
    Remove,
}

impl<T, N, Req, S> P2cPool<T, N, Req, S>
where
    T: Clone + Eq,
    N: NewService<(SocketAddr, T), Service = S>,
    S: Service<Req> + Load,
    S::Error: Into<Error>,
    S::Metric: std::fmt::Debug,
{
    pub fn new(metrics: P2cMetrics, new_endpoint: N) -> Self {
        let rng = SmallRng::from_rng(&mut rand::rng());
        Self {
            rng,
            metrics,
            new_endpoint,
            next_idx: None,
            pool: ReadyCache::default(),
            endpoints: Default::default(),
        }
    }

    fn p2c_ready_index(&mut self) -> Option<usize> {
        match self.pool.ready_len() {
            0 => None,
            1 => Some(0),
            len => {
                let (aidx, bidx) = gen_pair(&mut self.rng, len);
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

fn gen_pair(rng: &mut SmallRng, len: usize) -> (usize, usize) {
    debug_assert!(len >= 2, "must have at least two endpoints");
    // Get two distinct random indexes (in a random order) and
    // compare the loads of the service at each index.
    let aidx = rng.random_range(0..len);
    let mut bidx = rng.random_range(0..(len - 1));
    if bidx >= aidx {
        bidx += 1;
    }
    debug_assert_ne!(aidx, bidx, "random indices must be distinct");
    (aidx, bidx)
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
    fn reset_pool(&mut self, update: Vec<(SocketAddr, T)>) {
        let mut changed = false;
        let mut remaining = std::mem::take(&mut self.endpoints);
        for (addr, target) in update.into_iter() {
            let t = remaining.remove(&addr);
            if t.as_ref() == Some(&target) {
                tracing::debug!(?addr, "Endpoint unchanged");
            } else {
                if t.is_none() {
                    tracing::info!(?addr, "Adding endpoint");
                } else {
                    tracing::info!(?addr, "Updating endpoint");
                }

                let svc = self.new_endpoint.new_service((addr, target.clone()));
                self.pool.push(addr, svc);
                changed = true;
            }

            self.endpoints.insert(addr, target);
        }

        for (addr, _) in remaining.drain() {
            tracing::info!(?addr, "Removing endpoint");
            self.pool.evict(&addr);
            changed = true;
        }

        if changed {
            self.metrics.endpoints.set(self.endpoints.len() as i64);
            self.metrics.updates_reset.inc();
            self.next_idx = None;
        }
    }

    fn add_endpoint(&mut self, addr: SocketAddr, target: T) {
        match self.endpoints.entry(addr) {
            Entry::Occupied(e) if e.get() == &target => {
                tracing::debug!(?addr, "Endpoint unchanged");
                return;
            }
            Entry::Occupied(mut e) => {
                e.insert(target.clone());
            }
            Entry::Vacant(e) => {
                e.insert(target.clone());
                self.metrics.endpoints.inc();
            }
        }

        tracing::info!(?addr, "Adding endpoint");
        let svc = self.new_endpoint.new_service((addr, target));
        self.pool.push(addr, svc);
        self.metrics.updates_add.inc();
    }

    fn remove_endpoint(&mut self, addr: SocketAddr) {
        if self.endpoints.remove(&addr).is_none() {
            tracing::debug!(?addr, "Unknown endpoint");
            return;
        }

        tracing::info!(?addr, "Removing endpoint");
        self.pool.evict(&addr);
        self.metrics.endpoints.dec();
        self.metrics.updates_rm.inc();
        self.next_idx = None;
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
    /// cases, the caller must add endpoints and then wait for new endpoints to
    /// become ready.
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

impl<T, N, Req, S> Drop for P2cPool<T, N, Req, S> {
    fn drop(&mut self) {
        self.metrics.endpoints.set(0);
    }
}

// === impl P2cMetricFamilies ===

impl<L> Default for P2cMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            endpoints: prom::Family::default(),
            updates: prom::Family::default(),
        }
    }
}

impl<L> P2cMetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::registry::Registry) -> Self {
        let endpoints = prom::Family::default();
        reg.register(
            "endpoints",
            "The number of endpoints currently in the balancer",
            endpoints.clone(),
        );

        let updates = prom::Family::default();
        reg.register(
            "updates",
            "The total number of service discovery updates received by a balancer",
            updates.clone(),
        );

        Self { endpoints, updates }
    }

    pub fn metrics(&self, labels: &L) -> P2cMetrics {
        let endpoints: prom::Gauge = self.endpoints.get_or_create(labels).clone();
        let updates_reset: prom::Counter = self
            .updates
            .get_or_create(&UpdateLabels {
                op: UpdateOp::Reset,
                labels: labels.clone(),
            })
            .clone();
        let updates_add: prom::Counter = self
            .updates
            .get_or_create(&UpdateLabels {
                op: UpdateOp::Add,
                labels: labels.clone(),
            })
            .clone();
        let updates_rm: prom::Counter = self
            .updates
            .get_or_create(&UpdateLabels {
                op: UpdateOp::Remove,
                labels: labels.clone(),
            })
            .clone();
        P2cMetrics {
            endpoints,
            updates_reset,
            updates_add,
            updates_rm,
        }
    }
}

// === impl P2cMetrics ===

impl<L: prom::encoding::EncodeLabelSet> prom::encoding::EncodeLabelSet for UpdateLabels<L> {
    fn encode(&self, mut enc: prom::encoding::LabelSetEncoder<'_>) -> std::fmt::Result {
        use prom::encoding::EncodeLabel;
        ("op", self.op).encode(enc.encode_label())?;
        self.labels.encode(enc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahash::HashSet;
    use linkerd_stack::ServiceExt;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::time;
    use tokio_test::{assert_pending, assert_ready_ok};
    use tower::load::{CompleteOnResponse, PeakEwma};

    quickcheck::quickcheck! {
        fn gen_pair_distinct(len: usize) -> quickcheck::TestResult {
            if len < 2 {
                return quickcheck::TestResult::discard();
            }
            let mut rng = SmallRng::from_rng(&mut rand::rng());
            let (aidx, bidx) = gen_pair(&mut rng, len);
            quickcheck::TestResult::from_bool(aidx != bidx)
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn update_pool() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let addr0 = "192.168.10.10:80".parse().unwrap();
        let addr1 = "192.168.10.11:80".parse().unwrap();

        let seen = Arc::new(Mutex::new(HashSet::<(SocketAddr, usize)>::default()));
        let metrics = P2cMetrics::default();
        let mut pool = P2cPool::new(metrics.clone(), |(addr, n): (SocketAddr, usize)| {
            assert!(seen.lock().insert((addr, n)));
            PeakEwma::new(
                linkerd_stack::service_fn(|()| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(()))
                }),
                time::Duration::from_secs(1),
                1.0 * 1000.0 * 1000.0,
                CompleteOnResponse::default(),
            )
        });

        pool.reset_pool(vec![(addr0, 0)]);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&0));

        pool.add_endpoint(addr0, 1);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&1));

        pool.reset_pool(vec![(addr0, 1)]);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&1));

        pool.add_endpoint(addr1, 1);
        assert_eq!(pool.endpoints.len(), 2);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr1), Some(&1));

        pool.add_endpoint(addr1, 1);
        assert_eq!(pool.endpoints.len(), 2);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr1), Some(&1));

        pool.remove_endpoint(addr0);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);

        pool.remove_endpoint(addr0);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);

        pool.reset_pool(vec![(addr0, 2), (addr1, 2)]);
        assert_eq!(pool.endpoints.len(), 2);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&2));
        assert_eq!(pool.endpoints.get(&addr1), Some(&2));

        pool.reset_pool(vec![(addr0, 2)]);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&2));

        pool.reset_pool(vec![(addr0, 3)]);
        assert_eq!(pool.endpoints.len(), 1);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
        assert_eq!(pool.endpoints.get(&addr0), Some(&3));

        pool.reset_pool(vec![]);
        assert_eq!(pool.endpoints.len(), 0);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);

        pool.reset_pool(vec![]);
        assert_eq!(pool.endpoints.len(), 0);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);

        assert_eq!(metrics.updates_reset.get(), 5);
        assert_eq!(metrics.updates_add.get(), 2);
        assert_eq!(metrics.updates_rm.get(), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn update_pool_handle_drop() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let addr0 = "192.168.10.10:80".parse().unwrap();
        let addr1 = "192.168.10.11:80".parse().unwrap();
        let addr2 = "192.168.10.12:80".parse().unwrap();

        let seen = Arc::new(Mutex::new(HashSet::<(SocketAddr, usize)>::default()));
        let metrics = P2cMetrics::default();
        let mut pool = P2cPool::new(metrics.clone(), |(addr, n): (SocketAddr, usize)| {
            assert!(seen.lock().insert((addr, n)));
            PeakEwma::new(
                linkerd_stack::service_fn(|()| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(()))
                }),
                time::Duration::from_secs(1),
                1.0 * 1000.0 * 1000.0,
                CompleteOnResponse::default(),
            )
        });

        pool.reset_pool(vec![(addr0, 0), (addr1, 0), (addr2, 0)]);
        assert_eq!(pool.endpoints.len(), 3);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);

        drop(pool);
        assert_eq!(metrics.endpoints.get(), 0);

        let mut pool = P2cPool::new(metrics.clone(), |(addr, n): (SocketAddr, usize)| {
            assert!(seen.lock().insert((addr, n)));
            PeakEwma::new(
                linkerd_stack::service_fn(|()| {
                    std::future::ready(Ok::<_, std::convert::Infallible>(()))
                }),
                time::Duration::from_secs(1),
                1.0 * 1000.0 * 1000.0,
                CompleteOnResponse::default(),
            )
        });
        pool.reset_pool(vec![(addr0, 1), (addr1, 1), (addr2, 1)]);
        assert_eq!(pool.endpoints.len(), 3);
        assert_eq!(metrics.endpoints.get(), pool.endpoints.len() as i64);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn p2c_ready_index() {
        let _trace = linkerd_tracing::test::with_default_filter("trace");

        let addr0 = "192.168.10.10:80".parse().unwrap();
        let (svc0, mut h0) = tower_test::mock::pair::<(), ()>();
        h0.allow(0);

        let addr1 = "192.168.10.11:80".parse().unwrap();
        let (svc1, mut h1) = tower_test::mock::pair::<(), ()>();
        h1.allow(0);

        let addr2 = "192.168.10.12:80".parse().unwrap();
        let (svc2, mut h2) = tower_test::mock::pair::<(), ()>();
        h2.allow(0);

        let metrics = P2cMetrics::default();
        let mut pool = P2cPool::new(metrics, |(a, ())| {
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

        assert!(pool.ready().now_or_never().is_none());
        assert!(pool.next_idx.is_none());

        pool.reset_pool(vec![(addr0, ())]);
        assert!(pool.ready().now_or_never().is_none());
        assert!(pool.next_idx.is_none());

        h0.allow(1);
        assert!(pool.ready().now_or_never().is_some());
        assert_eq!(pool.next_idx, Some(0));

        h1.allow(1);
        h2.allow(1);
        pool.reset_pool(vec![(addr0, ()), (addr1, ()), (addr2, ())]);
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

        assert!(pool.ready().now_or_never().is_some());
        assert!(pool.next_idx.is_some());
        assert_eq!(pool.pool.ready_len(), 2);
        assert_eq!(pool.pool.pending_len(), 1);

        let ctx = &mut Context::from_waker(futures_util::task::noop_waker_ref());
        assert_pending!(pool.poll_pool(ctx));

        h0.allow(1);
        h1.allow(1);
        h2.allow(1);

        assert_ready_ok!(pool.poll_pool(ctx));

        assert!(pool.next_idx.is_some());
        assert_eq!(pool.pool.ready_len(), 3);
        assert_eq!(pool.pool.pending_len(), 0);
    }
}
