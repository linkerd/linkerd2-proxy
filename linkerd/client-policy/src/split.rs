use crate::LogicalAddr;
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
mod dynamic;
pub use self::dynamic::*;

pub fn layer<N, S, Req>() -> impl layer::Layer<N, Service = NewSplit<N, S, Req>> + Clone {
    // This RNG doesn't need to be cryptographically secure. Small and fast is
    // preferable.
    layer::mk(move |inner| NewSplit {
        inner,
        _service: PhantomData,
    })
}

/// A fixed (non-updating) traffic split.
#[derive(Debug)]
pub struct NewSplit<N, S, Req> {
    inner: N,
    _service: PhantomData<fn(Req) -> S>,
}

pub struct Split<S, Req> {
    rng: SmallRng,
    distribution: WeightedIndex<u32>,
    addrs: IndexSet<NameAddr>,
    services: ReadyCache<NameAddr, S, Req>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    pub weight: u32,
    pub addr: Addr,
}

// === impl NewSplit ===

impl<T, N, S, Req> NewService<T> for NewSplit<N, S, Req>
where
    T: Clone + Param<LogicalAddr> + Param<Vec<Backend>>,
    N: NewService<(ConcreteAddr, T), Service = S> + Clone,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Service = Split<S, Req>;

    fn new_service(&self, target: T) -> Self::Service {
        let backends: Vec<Backend> = target.param();
        Split::new(backends, &self.inner, target)
    }
}

impl<N: Clone, S, Req> Clone for NewSplit<N, S, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _service: PhantomData,
        }
    }
}

// === impl Split ===

impl<S, Req> Split<S, Req> {
    fn new<T>(
        mut backends: Vec<Backend>,
        new_service: &impl NewService<(ConcreteAddr, T), Service = S>,
        target: T,
    ) -> Self
    where
        S: tower::Service<Req>,
        S::Error: Into<Error>,
        T: Param<LogicalAddr> + Clone,
    {
        if backends.is_empty() {
            let LogicalAddr(addr) = target.param();
            backends.push(Backend {
                addr: addr.into(),
                weight: 1,
            })
        }
        tracing::trace!(?backends, "Building split service");

        let mut addrs = IndexSet::with_capacity(backends.len());
        let mut weights = Vec::with_capacity(backends.len());
        let mut services = ReadyCache::default();
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

        Self {
            services,
            addrs,
            distribution: WeightedIndex::new(weights).unwrap(),
            rng: SmallRng::from_rng(&mut thread_rng()).expect("RNG must initialize"),
        }
    }
}

impl<S, Req> tower::Service<Req> for Split<S, Req>
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
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
        tracing::trace!(?addr, "Dispatching");
        Box::pin(self.services.call_ready(addr, req).err_into::<Error>())
    }
}
