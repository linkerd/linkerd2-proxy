use crate::{Profile, Receiver, Target};
use futures::{prelude::*, ready};
use indexmap::IndexSet;
use linkerd2_addr::Addr;
use linkerd2_error::Error;
use linkerd2_stack::{layer, NewService};
use rand::distributions::{Distribution, WeightedIndex};
use rand::{rngs::SmallRng, SeedableRng};
use std::{
    fmt,
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

enum Inner<T, N, S, Req> {
    Default(S),
    Split {
        rng: SmallRng,
        rx: Pin<Box<dyn Stream<Item = Profile> + Send + Sync>>,
        target: T,
        new_service: N,
        distribution: WeightedIndex<u32>,
        addrs: IndexSet<Addr>,
        services: ReadyCache<Addr, S, Req>,
    },
}

// === impl NewSplit ===

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
    T: Clone,
    for<'t> &'t T: Into<Addr> + Into<Option<Receiver>>,
    N: NewService<(Option<Addr>, T), Service = S> + Clone,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Service = Split<T, N, S, Req>;

    fn new_service(&mut self, target: T) -> Self::Service {
        // If there is a profile, it is used to configure one or more inner
        // services and a concrete address is provided so that the endpoint
        // discovery is performed.
        //
        // Otherwise, profile lookup was rejected and, therefore, no concrete
        // address is provided.
        let inner = match Into::<Option<Receiver>>::into(&target) {
            None => {
                trace!("Building default service");
                Inner::Default(self.inner.new_service((None, target)))
            }
            Some(rx) => {
                let mut targets = rx.borrow().targets.clone();
                if targets.len() == 0 {
                    targets.push(Target {
                        addr: (&target).into(),
                        weight: 1,
                    })
                }
                trace!(?targets, "Building split service");

                let mut addrs = IndexSet::with_capacity(targets.len());
                let mut weights = Vec::with_capacity(targets.len());
                let mut services = ReadyCache::default();
                let mut new_service = self.inner.clone();
                for Target { weight, addr } in targets.into_iter() {
                    services.push(
                        addr.clone(),
                        new_service.new_service((Some(addr.clone()), target.clone())),
                    );
                    addrs.insert(addr);
                    weights.push(weight);
                }

                Inner::Split {
                    rx: crate::stream_profile(rx),
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

// === impl Split ===

impl<T, N, S, Req> tower::Service<Req> for Split<T, N, S, Req>
where
    Req: Send + 'static,
    T: Clone,
    for<'t> &'t T: Into<Addr>,
    N: NewService<(Option<Addr>, T), Service = S> + Clone,
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
                while let Poll::Ready(Some(up)) = rx.as_mut().poll_next(cx) {
                    update = Some(up.clone());
                }

                // Every time the profile updates, rebuild the distribution, reusing
                // services that existed in the prior state.
                if let Some(Profile { mut targets, .. }) = update {
                    if targets.len() == 0 {
                        targets.push(Target {
                            addr: target.into(),
                            weight: 1,
                        })
                    }
                    debug!(?targets, "Updating");

                    // Replace the old set of addresses with an empty set. The
                    // prior set is used to determine whether a new service
                    // needs to be created and what stale services should be
                    // removed.
                    let mut prior_addrs =
                        std::mem::replace(addrs, IndexSet::with_capacity(targets.len()));
                    let mut weights = Vec::with_capacity(targets.len());

                    // Create an updated distribution and set of services.
                    for Target { weight, addr } in targets.into_iter() {
                        // Reuse the prior services whenever possible.
                        if !prior_addrs.remove(&addr) {
                            debug!(%addr, "Creating target");
                            let svc = new_service.new_service((Some(addr.clone()), target.clone()));
                            services.push(addr.clone(), svc);
                        } else {
                            trace!(%addr, "Target already exists");
                        }
                        addrs.insert(addr);
                        weights.push(weight);
                    }

                    *distribution = WeightedIndex::new(weights).unwrap();

                    // Remove all prior services that did not exist in the new
                    // set of targets.
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

impl<T, N, S, Req> fmt::Debug for Inner<T, N, S, Req>
where
    T: fmt::Debug,
    N: fmt::Debug,
    S: fmt::Debug,
    Req: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inner::Default(ref s) => f.debug_tuple("Inner::Default").field(s).finish(),
            Inner::Split {
                rng,
                rx: _,
                target,
                new_service,
                distribution,
                addrs,
                services,
            } => f
                .debug_struct("Inner")
                .field("rng", rng)
                .field("rx", &format_args!("<dyn Stream>"))
                .field("target", target)
                .field("new_service", new_service)
                .field("distribution", distribution)
                .field("addrs", addrs)
                .field("services", services)
                .finish(),
        }
    }
}
