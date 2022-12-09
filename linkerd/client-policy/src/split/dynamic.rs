use super::{Backend, Split};
use crate::LogicalAddr;
use futures::prelude::*;
use indexmap::IndexSet;
use linkerd_addr::Addr;
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Param};
use rand::distributions::WeightedIndex;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, trace};

pub struct BackendStream(pub Pin<Box<dyn Stream<Item = Vec<Backend>> + Send + Sync + 'static>>);

/// Builds dynamically updated traffic splits.
#[derive(Debug)]
pub struct NewDynamicSplit<N, S, Req> {
    inner: N,
    _service: PhantomData<fn(Req) -> S>,
}

pub struct DynamicSplit<T, N, S, Req> {
    stream: Pin<Box<dyn Stream<Item = Vec<Backend>> + Send + Sync + 'static>>,
    target: T,
    new_service: N,
    split: Split<S, Req>,
}

// === impl NewDynamicSplit ===

impl<N: Clone, S, Req> Clone for NewDynamicSplit<N, S, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _service: self._service,
        }
    }
}

impl<N, S, Req> NewDynamicSplit<N, S, Req> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        // This RNG doesn't need to be cryptographically secure. Small and fast is
        // preferable.
        layer::mk(move |inner| NewDynamicSplit {
            inner,
            _service: PhantomData,
        })
    }
}

impl<T, N, S, Req> NewService<T> for NewDynamicSplit<N, S, Req>
where
    T: Clone + Param<LogicalAddr> + Param<Vec<Backend>> + Param<BackendStream>,
    N: NewService<(Addr, T), Service = S> + Clone,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Service = DynamicSplit<T, N, S, Req>;

    fn new_service(&self, target: T) -> Self::Service {
        let backends: Vec<Backend> = target.param();
        let split = Split::new(backends, &self.inner, target.clone());
        let BackendStream(stream) = target.param();
        DynamicSplit {
            stream,
            target,
            new_service: self.inner.clone(),
            split,
        }
    }
}

// === impl DynamicSplit ===

impl<T, N, S, Req> tower::Service<Req> for DynamicSplit<T, N, S, Req>
where
    Req: Send + 'static,
    T: Clone + Param<LogicalAddr>,
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
            let mut prior_addrs = std::mem::replace(
                &mut self.split.addrs,
                IndexSet::with_capacity(backends.len()),
            );
            let mut weights = Vec::with_capacity(backends.len());

            // Create an updated distribution and set of services.
            for Backend { weight, addr } in backends.into_iter() {
                // Reuse the prior services whenever possible.
                if !prior_addrs.remove(&addr) {
                    debug!(%addr, "Creating backend");
                    let svc = self
                        .new_service
                        .new_service((addr.clone(), self.target.clone()));
                    self.split.services.push(addr.clone(), svc);
                } else {
                    trace!(%addr, "Backend already exists");
                }
                self.split.addrs.insert(addr);
                weights.push(weight);
            }

            self.split.distribution = WeightedIndex::new(weights).unwrap();

            // Remove all prior services that did not exist in the new
            // set of targets.
            for addr in prior_addrs.into_iter() {
                self.split.services.evict(&addr);
            }
        }

        self.split.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.split.call(req)
    }
}
