use super::{Receiver, Routes, WeightedAddr};
use futures::{future, prelude::*};
use indexmap::IndexMap;
use linkerd2_addr::Addr;
use linkerd2_error::Error;
use linkerd2_stack::{layer, NewService};
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

pub fn layer<N, S>(rng: SmallRng) -> impl layer::Layer<N, Service = NewSplit<N, S>> {
    layer::mk(move |inner| NewSplit {
        inner,
        rng: rng.clone(),
        _service: PhantomData,
    })
}

#[derive(Clone, Debug)]
pub struct NewSplit<N, S> {
    inner: N,
    rng: SmallRng,
    _service: PhantomData<fn() -> S>,
}

#[derive(Clone, Debug)]
pub struct Split<T, N, S> {
    target: T,
    rx: Receiver,
    new_service: N,
    rng: SmallRng,
    inner: Option<Inner<S>>,
}

#[derive(Clone, Debug)]
struct Inner<S> {
    distribution: WeightedIndex<u32>,
    services: IndexMap<Addr, S>,
}

impl<T, N: Clone, S> NewService<(Receiver, T)> for NewSplit<N, S> {
    type Service = Split<T, N, S>;

    fn new_service(&self, (rx, target): (Receiver, T)) -> Self::Service {
        Split {
            rx,
            target,
            new_service: self.inner.clone(),
            rng: self.rng.clone(),
            inner: None,
        }
    }
}

impl<Req, T, N, S> tower::Service<Req> for Split<T, N, S>
where
    Req: Send + 'static,
    T: Into<Addr> + Clone,
    N: NewService<(Addr, T), Service = S> + Clone,
    S: tower::Service<Req> + Clone + Send + 'static,
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
        if let Some(Routes { dst_overrides, .. }) = update {
            // Clear out the prior state and preserve its services for reuse.
            let mut prior = self.inner.take().map(|i| i.services).unwrap_or_default();

            let mut services = IndexMap::with_capacity(dst_overrides.len().max(1));
            let mut weights = Vec::with_capacity(dst_overrides.len().max(1));
            if dst_overrides.len() == 0 {
                // If there were no overrides, build a default backend from the
                // target.
                let addr: Addr = self.target.clone().into();
                let svc = prior.remove(&addr).unwrap_or_else(|| {
                    self.new_service
                        .new_service((addr.clone(), self.target.clone()))
                });
                services.insert(addr, svc);
                weights.push(1);
            } else {
                // Create an updated distribution and set of services.
                for WeightedAddr { weight, addr } in dst_overrides.into_iter() {
                    // Reuse the prior services whenever possible.
                    let svc = prior.remove(&addr).unwrap_or_else(|| {
                        self.new_service
                            .new_service((addr.clone(), self.target.clone()))
                    });
                    services.insert(addr, svc);
                    weights.push(weight);
                }
            }

            self.inner = WeightedIndex::new(weights).ok().map(|distribution| Inner {
                services,
                distribution,
            });
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if let Some(Inner {
            ref services,
            ref distribution,
        }) = self.inner.as_ref()
        {
            debug_assert_ne!(services.len(), 0);
            let idx = if services.len() == 1 {
                0
            } else {
                distribution.sample(&mut self.rng)
            };
            let (_, svc) = services.get_index(idx).expect("illegal service index");
            Box::pin(svc.clone().oneshot(req).err_into::<Error>())
        } else {
            Box::pin(future::err(NoTargets(()).into()))
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct NoTargets(());

impl std::fmt::Display for NoTargets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no targets available")
    }
}

impl std::error::Error for NoTargets {}
