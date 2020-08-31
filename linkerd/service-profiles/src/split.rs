use crate::{Profile, Receiver, Target};
use futures::{prelude::*, ready};
use indexmap::IndexSet;
use linkerd2_addr::Addr;
use linkerd2_error::Error;
use linkerd2_stack::{layer, NewService};
use rand::distributions::{Distribution, WeightedIndex};
use rand::{rngs::SmallRng, SeedableRng};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ready_cache::ReadyCache;
use tracing::{debug, trace};

pub fn layer<N, S, Req>() -> impl layer::Layer<N, Service = NewSplit<N, S, Req>> + Clone {
    let rng = SmallRng::from_entropy();
    layer::mk(move |inner| NewSplit {
        inner,
        rng: rng.clone(),
        _service: PhantomData,
    })
}

#[derive(Debug)]
pub struct NewSplit<N, S, Req> {
    inner: N,
    rng: SmallRng,
    _service: PhantomData<fn(Req) -> S>,
}

#[derive(Debug)]
pub struct Split<T, N, S, Req> {
    target: T,
    rx: Receiver,
    new_service: N,
    rng: SmallRng,
    inner: Option<Inner>,
    services: ReadyCache<Addr, S, Req>,
}

#[derive(Debug)]
struct Inner {
    distribution: WeightedIndex<u32>,
    addrs: IndexSet<Addr>,
}

impl<N: Clone, S, Req> Clone for NewSplit<N, S, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            rng: self.rng.clone(),
            _service: self._service,
        }
    }
}

impl<T, N: Clone, S, Req> NewService<(Receiver, T)> for NewSplit<N, S, Req>
where
    S: tower::Service<Req>,
{
    type Service = Split<T, N, S, Req>;

    fn new_service(&self, (rx, target): (Receiver, T)) -> Self::Service {
        Split {
            rx,
            target,
            new_service: self.inner.clone(),
            rng: self.rng.clone(),
            inner: None,
            services: ReadyCache::default(),
        }
    }
}

impl<T, N, S, Req> tower::Service<Req> for Split<T, N, S, Req>
where
    Req: Send + 'static,
    T: AsRef<Addr> + Clone,
    N: NewService<(Addr, T), Service = S> + Clone,
    S: tower::Service<Req> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut update = None;
        while let Poll::Ready(Some(up)) = self.rx.poll_recv_ref(cx) {
            update = Some(up.clone());
        }
        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Some(Profile { targets, .. }) = update {
            debug!(?targets, "Updating");
            self.update_inner(targets);
        }

        // If, somehow, the watch hasn't been notified at least once, build the
        // default target. This shouldn't actually be exercised, though.
        if self.inner.is_none() {
            self.update_inner(Vec::new());
        }
        debug_assert_ne!(self.services.len(), 0);

        // Wait for all target services to be ready. If any services fail, then
        // the whole service fails.
        Poll::Ready(ready!(self.services.poll_pending(cx)).map_err(Into::into))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let Inner {
            ref addrs,
            ref distribution,
        } = self.inner.as_ref().expect("Called before ready");
        debug_assert_ne!(addrs.len(), 0, "addrs empty");
        debug_assert_eq!(self.services.len(), addrs.len());

        let idx = if addrs.len() == 1 {
            0
        } else {
            distribution.sample(&mut self.rng)
        };
        let addr = addrs.get_index(idx).expect("invalid index");
        trace!(%addr, "Dispatching");
        Box::pin(self.services.call_ready(addr, req).err_into::<Error>())
    }
}

impl<T, N, S, Req> Split<T, N, S, Req>
where
    Req: Send + 'static,
    T: AsRef<Addr> + Clone,
    N: NewService<(Addr, T), Service = S> + Clone,
    S: tower::Service<Req> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    fn update_inner(&mut self, targets: Vec<Target>) {
        // Clear out the prior state and preserve its services for reuse.
        let mut prior = self.inner.take().map(|i| i.addrs).unwrap_or_default();

        let mut addrs = IndexSet::with_capacity(targets.len().max(0));
        let mut weights = Vec::with_capacity(targets.len().max(1));
        if targets.len() == 0 {
            // If there were no overrides, build a default backend from the
            // target.
            let addr = self.target.as_ref();
            if !prior.remove(addr) {
                debug!(%addr, "Creating default target");
                let svc = self
                    .new_service
                    .new_service((addr.clone(), self.target.clone()));
                self.services.push(addr.clone(), svc);
            } else {
                debug!(%addr, "Default target already exists");
            }
            addrs.insert(addr.clone());
            weights.push(1);
        } else {
            // Create an updated distribution and set of services.
            for Target { weight, addr } in targets.into_iter() {
                // Reuse the prior services whenever possible.
                if !prior.remove(&addr) {
                    debug!(%addr, "Creating target");
                    let svc = self
                        .new_service
                        .new_service((addr.clone(), self.target.clone()));
                    self.services.push(addr.clone(), svc);
                } else {
                    debug!(%addr, "Target already exists");
                }
                addrs.insert(addr);
                weights.push(weight);
            }
        }

        for addr in prior {
            self.services.evict(&addr);
        }
        if !addrs.contains(self.target.as_ref()) {
            self.services.evict(self.target.as_ref());
        }

        debug_assert_ne!(addrs.len(), 0, "addrs empty");
        debug_assert_eq!(addrs.len(), weights.len(), "addrs does not match weights");
        // The cache may still contain evicted pending services until the next
        // poll.
        debug_assert!(
            addrs.len() <= self.services.len(),
            "addrs does not match the number of services"
        );
        let distribution = WeightedIndex::new(weights).expect("Split must be valid");
        self.inner = Some(Inner {
            addrs,
            distribution,
        });
    }
}
