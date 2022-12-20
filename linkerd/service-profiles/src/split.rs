use crate::{LogicalAddr, Profile, Receiver, ReceiverStream, Target};
use futures::{prelude::*, ready};
use indexmap::IndexSet;
use linkerd_addr::NameAddr;
use linkerd_error::Error;
use linkerd_proxy_api_resolve::ConcreteAddr;
use linkerd_stack::{layer, NewService, Param, Service};
use rand::distributions::{Distribution, WeightedIndex};
use rand::{rngs::SmallRng, thread_rng, SeedableRng};
use std::{
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use tower::ready_cache::ReadyCache;
use tracing::{debug, trace};

#[derive(Debug)]
pub struct NewDistribute<N, Req> {
    inner: N,
    _p: PhantomData<fn() -> Req>,
}

#[derive(Debug)]
pub struct Distribute<T, N, Req>
where
    N: NewService<(ConcreteAddr, T)>,
{
    target: T,
    new_inner: N,

    rng: SmallRng,
    rx: ReceiverStream,
    addrs: IndexSet<NameAddr>,
    services: ReadyCache<NameAddr, N::Service, Req>,
    distribution: Option<Arc<WeightedIndex<u32>>>,
}

// === impl NewDistribute ===

impl<N, Req> NewDistribute<N, Req> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(|inner| Self {
            inner,
            _p: PhantomData,
        })
    }
}

impl<N: Clone, Req> Clone for NewDistribute<N, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, N, S, Req> NewService<T> for NewDistribute<N, Req>
where
    T: Param<Receiver> + Clone,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
    S: Service<Req, Error = Error>,
{
    type Service = Distribute<T, N, Req>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx: Receiver = target.param();
        Self::Service {
            target,
            new_inner: self.inner.clone(),

            rng: SmallRng::from_rng(&mut thread_rng()).expect("rng"),
            rx: rx.into(),
            addrs: Default::default(),
            services: Default::default(),
            distribution: None,
        }
    }
}

// === impl Distribute ===

impl<T: Clone, N: Clone, Req> Clone for Distribute<T, N, Req>
where
    N: NewService<(ConcreteAddr, T)>,
    N::Service: Service<Req>,
{
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            new_inner: self.new_inner.clone(),

            rng: SmallRng::from_rng(&mut thread_rng()).expect("rng"),
            rx: self.rx.clone(),
            addrs: Default::default(),
            services: Default::default(),
            distribution: None,
        }
    }
}

impl<T, N, S, Req> Distribute<T, N, Req>
where
    Req: Send + 'static,
    T: Clone + Param<LogicalAddr>,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
    S: tower::Service<Req> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    fn update(&mut self, mut targets: Vec<Target>) {
        if targets.is_empty() {
            let LogicalAddr(addr) = self.target.param();
            targets.push(Target { addr, weight: 1 })
        }
        debug!(?targets, "Updating");

        if targets.len() == self.addrs.len() || targets.iter().all(|t| self.addrs.contains(&t.addr))
        {
            return;
        }

        // Replace the old set of addresses with an empty set. The
        // prior set is used to determine whether a new service
        // needs to be created and what stale services should be
        // removed.
        let mut prior_addrs =
            std::mem::replace(&mut self.addrs, IndexSet::with_capacity(targets.len()));
        let mut weights = Vec::with_capacity(targets.len());

        // Create an updated distribution and set of services.
        for Target { weight, addr } in targets.into_iter() {
            // Reuse the prior services whenever possible.
            if !prior_addrs.remove(&addr) {
                debug!(%addr, "Creating target");
                let svc = self
                    .new_inner
                    .new_service((ConcreteAddr(addr.clone()), self.target.clone()));
                self.services.push(addr.clone(), svc);
            } else {
                trace!(%addr, "Target already exists");
            }
            self.addrs.insert(addr);
            weights.push(weight);
        }

        self.distribution = Some(Arc::new(WeightedIndex::new(weights).unwrap()));

        // Remove all prior services that did not exist in the new
        // set of targets.
        for addr in prior_addrs.into_iter() {
            self.services.evict(&addr);
        }
    }
}

impl<T, N, S, Req> tower::Service<Req> for Distribute<T, N, Req>
where
    Req: Send + 'static,
    T: Clone + Param<LogicalAddr>,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
    S: tower::Service<Req, Error = Error> + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Poll::Ready(Some(Profile { targets, .. })) = self.rx.poll_next_unpin(cx) {
            self.update(targets);
        }

        // Wait for all target services to be ready. If any services fail, then
        // the whole service fails.
        Poll::Ready(ready!(self.services.poll_pending(cx).map_err(Into::into)))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let addr = {
            let idx = if self.addrs.len() == 1 {
                0
            } else {
                self.distribution
                    .as_ref()
                    .expect("distribution must be set")
                    .sample(&mut self.rng)
            };
            self.addrs.get_index(idx).expect("invalid index")
        };
        trace!(?addr, "Dispatching");
        self.services.call_ready(addr, req)
    }
}
