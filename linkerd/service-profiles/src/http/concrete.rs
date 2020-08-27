use super::{Receiver, Routes, WeightedAddr};
use futures::{future, prelude::*};
use indexmap::IndexMap;
use linkerd2_addr::Addr;
use linkerd2_error::{Error, Never};
use linkerd2_stack::NewService;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;
use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

pub struct MakeSplit<N, S> {
    new_service: N,
    rng: SmallRng,
    _service: PhantomData<S>,
}

pub struct Split<T, N, S> {
    target: T,
    rx: Receiver,
    new_service: N,
    rng: SmallRng,
    inner: Option<Inner<S>>,
}

struct Inner<S> {
    distribution: WeightedIndex<u32>,
    services: IndexMap<Addr, S>,
}

impl<T, N: Clone, S> tower::Service<(T, Receiver)> for MakeSplit<N, S> {
    type Response = Split<T, N, S>;
    type Error = Never;
    type Future = future::Ready<Result<Split<T, N, S>, Never>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (target, rx): (T, Receiver)) -> Self::Future {
        future::ok(Split {
            rx,
            target,
            new_service: self.new_service.clone(),
            rng: self.rng.clone(),
            inner: None,
        })
    }
}

impl<Req, T, N, S> tower::Service<Req> for Split<T, N, S>
where
    Req: Send + 'static,
    T: Clone,
    N: NewService<(T, Addr), Service = S> + Clone,
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
        if let Some(Routes { dst_overrides, .. }) = update {
            self.inner = if dst_overrides.len() == 0 {
                None
            } else {
                let mut services = IndexMap::with_capacity(dst_overrides.len());
                let mut weights = Vec::with_capacity(dst_overrides.len());
                let mut prior = self.inner.take().map(|i| i.services).unwrap_or_default();
                for WeightedAddr { weight, addr } in dst_overrides.into_iter() {
                    let svc = prior.remove(&addr).unwrap_or_else(|| {
                        self.new_service
                            .new_service((self.target.clone(), addr.clone()))
                    });
                    services.insert(addr, svc);
                    weights.push(weight);
                }
                WeightedIndex::new(weights).ok().map(|distribution| Inner {
                    services,
                    distribution,
                })
            };
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
            let service = if services.len() == 1 {
                services.get_index(0).unwrap().1.clone()
            } else {
                let idx = distribution.sample(&mut self.rng);
                services.get_index(idx).unwrap().1.clone()
            };
            return Box::pin(service.oneshot(req).err_into::<Error>());
        }

        return Box::pin(future::err(NoTargets(()).into()));
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
