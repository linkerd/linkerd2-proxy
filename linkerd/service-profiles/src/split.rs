//! A general load-agnostic traffic distribution stack.
//!
//! TODO(ver) This is totally decoupled from service profiles and should live
//! somewhere else.

use futures::prelude::*;
use indexmap::IndexMap;
use linkerd_stack::{NewService, Param, Service};
use rand::{
    distributions::{Distribution as _, WeightedIndex},
    rngs::SmallRng,
    SeedableRng,
};
use std::{
    collections::HashSet,
    hash::Hash,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::warn;

#[derive(Clone, Debug)]
pub struct NewDistribute<K, S> {
    backends: Arc<IndexMap<K, S>>,
}

#[derive(Debug)]
pub struct Distribute<K, S> {
    backends: Arc<IndexMap<K, S>>,
    ready_idx: Option<usize>,
    inner: Inner<K>,
}

/// A parameter type that configures how a [`Distribute`] should behave.
#[derive(Clone, Debug)]
pub enum Distribution<K> {
    /// A distribution that has no backends, and therefore never becomes ready.
    Empty,

    /// A distribution that uses the first available backend in an ordered list.
    FirstAvailable(Arc<[K]>),

    /// A distribution that uses the first available backend when randomly
    /// selecting over a weighted distributino of backends.
    RandomAvailable(Arc<WeightedKeys<K>>),
}

#[derive(Debug)]
pub struct WeightedKeys<K> {
    keys: Vec<K>,
    index: WeightedIndex<usize>,
}

#[derive(Debug)]
enum Inner<K> {
    Empty,
    FirstAvailable {
        keys: Arc<[K]>,
    },
    RandomAvailable {
        keys: Arc<WeightedKeys<K>>,
        polled_idxs: HashSet<usize>,
        rng: SmallRng,
    },
}

// === impl NewDistribute ===

impl<K: Hash + Eq, S> NewDistribute<K, S> {
    #[inline]
    pub(crate) fn new(backends: impl IntoIterator<Item = (K, S)>) -> Self {
        let backends = backends.into_iter().collect();
        assert!(
            !backends.is_empty(),
            "must have at least one concrete backend"
        );
        Self { backends }
    }
}

impl<T, K, S> NewService<T> for NewDistribute<K, S>
where
    T: Param<Distribution<K>>,
    K: Hash + Eq,
    S: Clone,
{
    type Service = Distribute<K, S>;

    fn new_service(&self, target: T) -> Self::Service {
        Self::Service {
            inner: target.param().into(),
            ready_idx: None,
            backends: self.backends.clone(),
        }
    }
}

// === impl Distribute ===

impl<K, S> Clone for Distribute<K, S> {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            // Clear the ready index so that the new clone must become ready
            // independently.
            ready_idx: None,
            inner: self.inner.clone(),
        }
    }
}

impl<K> Clone for Inner<K> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Inner::Empty,
            Self::FirstAvailable { keys } => Self::FirstAvailable { keys: keys.clone() },
            Self::RandomAvailable { keys, .. } => Self::RandomAvailable {
                keys: keys.clone(),
                polled_idxs: HashSet::with_capacity(keys.keys.len()),
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}

impl<Req, K, S> Service<Req> for Distribute<K, S>
where
    K: Hash + Eq + std::fmt::Debug,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    // Note that this doesn't necessarily drive all inner services to readiness.
    // We expect that these inner services should be buffered or otherwise drive
    // themselves to readiness (i.e. via SpawnReady).
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If we've already chosen a ready index, then skip polling.
        if self.ready_idx.is_some() {
            return Poll::Ready(Ok(()));
        }

        match self.inner {
            // Empty distributions are never ready.
            Inner::Empty => {}

            Inner::FirstAvailable { ref keys } => {
                for key in keys.iter() {
                    let (idx, _, svc) = self
                        .backends
                        .get_full_mut(key)
                        .expect("distributions must not reference unknown backends");
                    if svc.poll_ready(cx)?.is_ready() {
                        self.ready_idx = Some(idx);
                        return Poll::Ready(Ok(()));
                    }
                }
            }

            Inner::RandomAvailable {
                ref keys,
                ref mut polled_idxs,
                ref mut rng,
            } => {
                // Choose a random index (via the weighted distribution) to try
                // to poll the backend. Continue selecting endpoints until we
                // find one that is ready or we've tried all backends in the
                // distribution.
                self.polled_idxs.clear();
                while polled_idxs.len() != keys.keys.len() {
                    let (idx, _, svc) = self
                        .backends
                        .get_full_mut(keys.next(rng))
                        .expect("distributions must not reference unknown backends");
                    if polled_idxs.insert(idx) {
                        // The index was not already polled, so poll it.
                        if svc.poll_ready(cx)?.is_ready() {
                            self.ready_idx = Some(idx);
                            return Poll::Ready(Ok(()));
                        }
                    }
                }
            }
        }

        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self
            .ready_idx
            .take()
            .expect("poll_ready must be called first");
        let (_, svc) = self
            .backends
            .get_index_mut(idx)
            .expect("index must exist in distribution");
        svc.call(req)
    }
}

// === impl Distribution ===

impl<K> Distribution<K> {
    pub fn empty() -> Self {
        Self::Empty
    }

    pub fn first_available(keys: impl IntoIterator<Item = K>) -> Self {
        let keys = keys.collect();
        if keys.is_empty() {
            return Self::Empty;
        }
        Self::FirstAvailable(keys)
    }

    pub fn random_available<T: IntoIterator<Item = (K, u32)>>(iter: T) -> Self {
        let (keys, weights): (Vec<_>, Vec<_>) = iter.into_iter().filter(|(_, w)| *w > 0).unzip();
        if keys.len() < 2 {
            return Self::first_available(keys);
        }

        let index = WeightedIndex::new(weights).expect("must succeed");
        Self::RandomAvailable(Arc::new(Self { keys, index }))
    }
}

impl<K> From<Distribution<K>> for Inner<K> {
    fn from(self) -> Self {
        match self {
            Distribution::Empty => Self::Empty,
            Distribution::FirstAvailable(keys) => Self::FirstAvailable { keys },
            Distribution::RandomAvailable(keys) => Self::RandomAvailable {
                polled_idxs: HashSet::with_capacity(keys.keys.len()),
                keys,
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}

// === impl Random===

impl<K> WeightedKeys<K> {
    fn next<R: rand::Rng>(&self, rng: &mut R) -> &K {
        let idx = self.index.sample(rng);
        &self.keys[idx]
    }
}
