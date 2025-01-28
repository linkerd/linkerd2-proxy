use crate::{keys::KeyId, WeightedServiceKeys};
use ahash::HashMap;
use linkerd_stack::{NewService, Service};
use rand::{rngs::SmallRng, SeedableRng};
use std::{
    hash::Hash,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Debug)]
pub(crate) struct RandomAvailableSelection<K, S> {
    keys: Arc<WeightedServiceKeys<K>>,
    backends: HashMap<KeyId, S>,
    rng: SmallRng,

    /// Stores the index of the backend that has been polled to ready. The
    /// service at this index will be used on the next invocation of
    /// `Service::call`.
    ready_idx: Option<KeyId>,
}

fn new_rng() -> SmallRng {
    SmallRng::from_rng(&mut rand::rng())
}

impl<K, S> RandomAvailableSelection<K, S> {
    pub fn new<N>(keys: &Arc<WeightedServiceKeys<K>>, make_svc: N) -> Self
    where
        N: for<'a> NewService<&'a K, Service = S>,
    {
        Self {
            keys: keys.clone(),
            backends: keys
                .iter()
                .map(|&id| (id, make_svc.new_service(&keys.get(id).key)))
                .collect(),
            ready_idx: None,
            rng: new_rng(),
        }
    }

    #[cfg(test)]
    pub fn get_ready_idx(&self) -> Option<KeyId> {
        self.ready_idx
    }
}

impl<K, S: Clone> Clone for RandomAvailableSelection<K, S> {
    fn clone(&self) -> Self {
        Self {
            keys: self.keys.clone(),
            backends: self.backends.clone(),
            rng: new_rng(),
            // Clear the ready index so that the new clone must become ready
            // independently.
            ready_idx: None,
        }
    }
}

impl<Req, K, S> Service<Req> for RandomAvailableSelection<K, S>
where
    K: Hash + Eq,
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // If we've already chosen a ready index, then skip polling.
        if self.ready_idx.is_some() {
            return Poll::Ready(Ok(()));
        }

        let mut selector = self.keys.selector();
        loop {
            let id = selector.select_weighted(&mut self.rng);
            let svc = self
                .backends
                .get_mut(&id)
                .expect("distributions must not reference unknown backends");

            if svc.poll_ready(cx)?.is_ready() {
                self.ready_idx = Some(id);
                return Poll::Ready(Ok(()));
            }

            // Since the backend we just tried isn't ready, zero out the weight
            // so that it's not tried again in this round, i.e. subsequent calls
            // to `poll_ready` can try this backend again.
            match selector.disable_backend(id) {
                Ok(()) => {}
                Err(rand::distr::weighted::Error::InsufficientNonZero) => {
                    // There are no backends remaining.
                    break;
                }
                Err(error) => {
                    tracing::error!(%error, "unexpected error updating weights; giving up");
                    break;
                }
            }
        }

        debug_assert!(self.ready_idx.is_none());
        Poll::Pending
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let id = self
            .ready_idx
            .take()
            .expect("poll_ready must be called first");

        let svc = self.backends.get_mut(&id).expect("index must exist");

        svc.call(req)
    }
}
