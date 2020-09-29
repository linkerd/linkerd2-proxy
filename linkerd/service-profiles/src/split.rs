use crate::{Profile, Receiver, Target};
use futures::{prelude::*, ready};
use indexmap::IndexSet;
use linkerd2_addr::NameAddr;
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
    // This RNG doesn't need to be cryptographically secure. Small and fast is
    // preferable.
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
    inner: Inner<T, N, S, Req>,
}

#[derive(Debug)]
enum Inner<T, N, S, Req> {
    Default(S),
    Split {
        rng: SmallRng,
        rx: Receiver,
        target: T,
        new_service: N,
        distribution: WeightedIndex<u32>,
        addrs: IndexSet<Option<NameAddr>>,
        services: ReadyCache<Option<NameAddr>, S, Req>,
    },
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

impl<T, N, S, Req> NewService<T> for NewSplit<N, S, Req>
where
    T: AsRef<Option<Receiver>> + Clone,
    N: NewService<(Option<NameAddr>, T), Service = S> + Clone,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Service = Split<T, N, S, Req>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let inner = match target.as_ref().clone() {
            None => Inner::Default(self.inner.new_service((None, target))),
            Some(rx) => {
                let targets = rx.borrow().targets.clone();
                let mut new_service = self.inner.clone();

                let mut addrs = IndexSet::with_capacity(targets.len().max(1));
                let mut weights = Vec::with_capacity(targets.len().max(1));
                let mut services = ReadyCache::default();

                // Create an updated distribution and set of services.
                if targets.len() == 0 {
                    services.push(None, new_service.new_service((None, target.clone())));
                    addrs.insert(None);
                    weights.push(1);
                } else {
                    for Target { weight, name } in targets.into_iter() {
                        services.push(
                            Some(name.clone()),
                            new_service.new_service((Some(name.clone()), target.clone())),
                        );
                        addrs.insert(Some(name));
                        weights.push(weight);
                    }
                }

                Inner::Split {
                    rx,
                    target,
                    new_service,
                    services,
                    addrs,
                    distribution: WeightedIndex::new(weights).unwrap(),
                    rng: self.rng.clone(),
                }
            }
        };

        Split { inner }
    }
}

impl<T, N, S, Req> tower::Service<Req> for Split<T, N, S, Req>
where
    Req: Send + 'static,
    T: Clone,
    N: NewService<(Option<NameAddr>, T), Service = S> + Clone,
    S: tower::Service<Req> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner {
            Inner::Default(ref mut svc) => svc.poll_ready(cx).map_err(Into::into),
            Inner::Split {
                ref mut rx,
                ref mut services,
                ref mut addrs,
                ref mut distribution,
                ref mut new_service,
                ref target,
                ..
            } => {
                let mut update = None;
                while let Poll::Ready(Some(up)) = rx.poll_recv_ref(cx) {
                    update = Some(up.clone());
                }

                // Every time the profile updates, rebuild the distribution, reusing
                // services that existed in the prior state.
                if let Some(Profile { targets, .. }) = update {
                    debug!(?targets, "Updating");

                    let mut prior_addrs =
                        std::mem::replace(addrs, IndexSet::with_capacity(targets.len().max(1)));
                    let mut weights = Vec::with_capacity(targets.len().max(1));

                    if targets.len() == 0 {
                        // Reuse the prior services whenever possible.
                        if !prior_addrs.remove(&None) {
                            debug!("Creating default target");
                            let svc = new_service.new_service((None, target.clone()));
                            services.push(None, svc);
                        } else {
                            debug!("Default target already exists");
                        }
                        addrs.insert(None);
                        weights.push(1);
                    } else {
                        // Create an updated distribution and set of services.
                        for Target { weight, name } in targets.into_iter() {
                            let addr = Some(name.clone());
                            // Reuse the prior services whenever possible.
                            if !prior_addrs.remove(&addr) {
                                debug!(%name, "Creating target");
                                let svc = new_service.new_service((addr.clone(), target.clone()));
                                services.push(addr.clone(), svc);
                            } else {
                                trace!(%name, "Target already exists");
                            }
                            addrs.insert(addr);
                            weights.push(weight);
                        }
                    }

                    *distribution = WeightedIndex::new(weights).unwrap();

                    for addr in prior_addrs.into_iter() {
                        services.evict(&addr);
                    }
                }

                // Wait for all target services to be ready. If any services fail, then
                // the whole service fails.
                Poll::Ready(ready!(services.poll_pending(cx)).map_err(Into::into))
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.inner {
            Inner::Default(ref mut svc) => Box::pin(svc.call(req).err_into::<Error>()),
            Inner::Split {
                ref addrs,
                ref distribution,
                ref mut services,
                ref mut rng,
                ..
            } => {
                let idx = if addrs.len() == 1 {
                    0
                } else {
                    distribution.sample(rng)
                };
                let addr = addrs.get_index(idx).expect("invalid index");
                trace!(?addr, "Dispatching");
                Box::pin(services.call_ready(addr, req).err_into::<Error>())
            }
        }
    }
}
