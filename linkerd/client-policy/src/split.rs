use crate::{Backend, LogicalAddr};
use futures::{prelude::*, ready};
use indexmap::IndexSet;
use linkerd_addr::{Addr, NameAddr};
use linkerd_error::Error;
use linkerd_proxy_api_resolve::ConcreteAddr;
use linkerd_stack::{layer, NewService, Param};
use rand::distributions::{Distribution, WeightedIndex};
use rand::{rngs::SmallRng, thread_rng, SeedableRng};
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::ready_cache::ReadyCache;
use tracing::{debug, trace};

pub fn layer<N, S, Req>() -> impl layer::Layer<N, Service = NewSplit<N, S, Req>> + Clone {
    // This RNG doesn't need to be cryptographically secure. Small and fast is
    // preferable.
    layer::mk(move |inner| NewSplit {
        inner,
        _service: PhantomData,
    })
}

#[derive(Debug)]
pub struct NewSplit<N, S, Req> {
    inner: N,
    _service: PhantomData<fn(Req) -> S>,
}

pub struct Split<T, N, S, Req> {
    rng: SmallRng,
    stream: Pin<Box<dyn Stream<Item = Vec<Backend>> + Send + Sync + 'static>>,
    target: T,
    new_service: N,
    distribution: WeightedIndex<u32>,
    addrs: IndexSet<NameAddr>,
    services: ReadyCache<NameAddr, S, Req>,
}
pub struct BackendStream(pub Pin<Box<dyn Stream<Item = Vec<Backend>> + Send + Sync + 'static>>);

// === impl NewSplit ===

impl<N: Clone, S, Req> Clone for NewSplit<N, S, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _service: self._service,
        }
    }
}

impl<T, N, S, Req> NewService<T> for NewSplit<N, S, Req>
where
    T: Clone + Param<LogicalAddr> + Param<Vec<Backend>> + Param<BackendStream>,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Service = Split<T, N, S, Req>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut backends: Vec<Backend> = target.param();
        if backends.is_empty() {
            let LogicalAddr(addr) = target.param();
            backends.push(Backend {
                addr: addr.into(),
                weight: 1,
            })
        }
        trace!(?backends, "Building split service");

        let mut addrs = IndexSet::with_capacity(backends.len());
        let mut weights = Vec::with_capacity(backends.len());
        let mut services = ReadyCache::default();
        let new_service = self.inner.clone();
        for Backend { weight, addr } in backends.into_iter() {
            match addr {
                Addr::Name(addr) => {
                    services.push(
                        addr.clone(),
                        new_service.new_service((ConcreteAddr(addr.clone()), target.clone())),
                    );
                    addrs.insert(addr);
                    weights.push(weight);
                }
                Addr::Socket(_) => todo!("eliza: update splits to handle WeightedEndpoints"),
            }
        }

        let BackendStream(stream) = target.param();

        Split {
            stream,
            target,
            new_service,
            services,
            addrs,
            distribution: WeightedIndex::new(weights).unwrap(),
            rng: SmallRng::from_rng(&mut thread_rng()).expect("RNG must initialize"),
        }
    }
}

// === impl Split ===

impl<T, N, S, Req> tower::Service<Req> for Split<T, N, S, Req>
where
    Req: Send + 'static,
    T: Clone + Param<LogicalAddr>,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
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
        while let Poll::Ready(Some(up)) = self.stream.poll_next_unpin(cx) {
            update = Some(up);
        }

        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Some(mut backends) = update {
            if backends.is_empty() {
                let LogicalAddr(addr) = self.target.param();
                backends.push(Backend {
                    addr: addr.into(),
                    weight: 1,
                })
            }
            debug!(?backends, "Updating");

            // Replace the old set of addresses with an empty set. The
            // prior set is used to determine whether a new service
            // needs to be created and what stale services should be
            // removed.
            let mut prior_addrs =
                std::mem::replace(&mut self.addrs, IndexSet::with_capacity(backends.len()));
            let mut weights = Vec::with_capacity(backends.len());

            // Create an updated distribution and set of services.
            for Backend { weight, addr } in backends.into_iter() {
                let addr = match addr {
                    Addr::Name(addr) => addr,
                    Addr::Socket(_) => todo!("eliza: update splits to handle WeightedEndpoints"),
                };
                // Reuse the prior services whenever possible.
                if !prior_addrs.remove(&addr) {
                    debug!(%addr, "Creating backend");
                    let svc = self
                        .new_service
                        .new_service((ConcreteAddr(addr.clone()), self.target.clone()));
                    self.services.push(addr.clone(), svc);
                } else {
                    trace!(%addr, "Backend already exists");
                }
                self.addrs.insert(addr);
                weights.push(weight);
            }

            self.distribution = WeightedIndex::new(weights).unwrap();

            // Remove all prior services that did not exist in the new
            // set of targets.
            for addr in prior_addrs.into_iter() {
                self.services.evict(&addr);
            }
        }

        // Wait for all target services to be ready. If any services fail, then
        // the whole service fails.
        Poll::Ready(ready!(self.services.poll_pending(cx)).map_err(Into::into))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let idx = if self.addrs.len() == 1 {
            0
        } else {
            self.distribution.sample(&mut self.rng)
        };
        let addr = self.addrs.get_index(idx).expect("invalid index");
        trace!(?addr, "Dispatching");
        Box::pin(self.services.call_ready(addr, req).err_into::<Error>())
    }
}
