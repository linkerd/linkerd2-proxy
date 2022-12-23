use super::{Distribution, WeightedKeys};
use indexmap::IndexMap;
use linkerd_stack::Service;
use rand::{
    distributions::{Distribution as _, WeightedError},
    rngs::SmallRng,
    SeedableRng,
};
use std::{
    hash::Hash,
    sync::Arc,
    task::{Context, Poll},
};

/// A service that distributes requests over a set of backends.
#[derive(Debug)]
pub struct Distribute<K, S> {
    backends: IndexMap<K, S>,
    selection: Selection<K>,

    /// Stores the index of the backend that has been polled to ready. The
    /// service at this index will be used on the next invocation of
    /// `Service::call`.
    ready_idx: Option<usize>,
}

/// Holds per-distribution state for a [`Distribute`] service.
#[derive(Debug)]
enum Selection<K> {
    Empty,
    FirstAvailable,
    RandomAvailable {
        keys: Arc<WeightedKeys<K>>,
        rng: SmallRng,
    },
}

// === impl Distribute ===

impl<K, S> Distribute<K, S>
where
    K: Hash + Eq + Clone,
    S: Clone,
{
    pub(crate) fn new(backends: IndexMap<K, S>, dist: Distribution<K>) -> Self {
        Self {
            backends,
            selection: dist.into(),
            ready_idx: None,
        }
    }
}

impl<Req, K, S> Service<Req> for Distribute<K, S>
where
    K: Hash + Eq,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    /// Acquires a ready backend.
    ///
    /// Note that this doesn't necessarily drive all backend services to
    /// readiness. We expect that these inner services should be buffered or
    /// otherwise drive themselves to readiness (i.e. via SpawnReady).
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If we've already chosen a ready index, then skip polling.
        if self.ready_idx.is_some() {
            return Poll::Ready(Ok(()));
        }

        match self.selection {
            Selection::Empty => {
                tracing::debug!("empty distribution will never become ready");
            }

            Selection::FirstAvailable => {
                for (idx, svc) in self.backends.values_mut().enumerate() {
                    if svc.poll_ready(cx)?.is_ready() {
                        self.ready_idx = Some(idx);
                        return Poll::Ready(Ok(()));
                    }
                }
            }

            Selection::RandomAvailable {
                ref keys,
                ref mut rng,
            } => {
                // Choose a random index (via the weighted distribution) to try
                // to poll the backend. Continue selecting endpoints until we
                // find one that is ready or we've tried all backends in the
                // distribution.
                let mut index = keys.index().clone();
                loop {
                    // Sample the weighted index to find a backend to try.
                    let idx = index.sample(rng);
                    let (_, svc) = self
                        .backends
                        .get_index_mut(idx)
                        .expect("distributions must not reference unknown backends");

                    if svc.poll_ready(cx)?.is_ready() {
                        self.ready_idx = Some(idx);
                        return Poll::Ready(Ok(()));
                    }

                    // Zero out the weight of the backend we just tried so that
                    // it's not selected again.
                    match index.update_weights(&[(idx, &0)]) {
                        Ok(()) => {}
                        Err(WeightedError::AllWeightsZero) => {
                            // If zeroeing out the index puts it into an invalid
                            // state, then there are no backends remaining.
                            break;
                        }
                        Err(error) => {
                            tracing::error!(%error, "unexpected error updating weights; giving up");
                            break;
                        }
                    }
                }
            }
        }

        debug_assert!(self.ready_idx.is_none());
        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = self
            .ready_idx
            .take()
            .expect("poll_ready must be called first");

        let (_, svc) = self.backends.get_index_mut(idx).expect("index must exist");

        svc.call(req)
    }
}

impl<K: Clone, S: Clone> Clone for Distribute<K, S> {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            selection: self.selection.clone(),
            // Clear the ready index so that the new clone must become ready
            // independently.
            ready_idx: None,
        }
    }
}

// === impl Selection ===

impl<K> From<Distribution<K>> for Selection<K> {
    fn from(dist: Distribution<K>) -> Self {
        match dist {
            Distribution::Empty => Self::Empty,
            Distribution::FirstAvailable(_) => Self::FirstAvailable,
            Distribution::RandomAvailable(keys) => Self::RandomAvailable {
                keys,
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}

impl<K> Clone for Selection<K> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Selection::Empty,
            Self::FirstAvailable => Self::FirstAvailable,
            Self::RandomAvailable { keys, .. } => Self::RandomAvailable {
                keys: keys.clone(),
                rng: SmallRng::from_rng(rand::thread_rng()).expect("RNG must initialize"),
            },
        }
    }
}
