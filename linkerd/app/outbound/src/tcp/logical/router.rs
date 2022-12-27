use futures::prelude::*;
use linkerd_app_core::{
    profiles,
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewCloneService, NewService, Oneshot, Param, Service, ServiceExt},
    NameAddr,
};
use std::{
    collections::HashMap,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::{error, trace, Instrument};

#[derive(Clone, Debug)]
pub struct NewRouter<N, U>(N, std::marker::PhantomData<fn(U)>);

#[derive(Clone, Debug)]
pub struct Router<S> {
    rx: watch::Receiver<Distribute<S>>,
    current: Distribute<S>,
}

type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;

// === impl NewServiceRouter ===

impl<N, U> NewRouter<N, U> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(|inner| Self(inner, std::marker::PhantomData))
    }
}

impl<T, U, N> NewService<T> for NewRouter<N, U>
where
    T: Param<profiles::LogicalAddr> + Param<profiles::Receiver>,
    T: Clone + Send + 'static,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U> + Clone + Send + 'static,
    N::Service: Clone + Send + Sync + 'static,
{
    type Service = Router<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        // Spawn a background task that watches for profile updates and, when a
        // change is necessary, rebuilds stacks:
        //
        // 1. Maintain a cache of backend backend services (i.e., load
        //    balancers). These services are shared across all routes and
        //    therefor must be cloneable (i.e., buffered).
        // 2.
        // 3. Publish these stacks so that they may be used

        let mut profiles: profiles::Receiver = target.param();
        // Build the initial stacks by checking the profile.
        let profile = profiles.borrow_and_update();

        // TODO(ver) use a different key type that is structured instead of
        // simply a name.
        let mut backends = HashMap::with_capacity(profile.targets.len().max(1));
        Self::mk_backends(&mut backends, &self.0, &profile.targets, &target);

        // Create a stack that can distribute requests to the backends.
        let distribute = Self::mk_distribute(&backends, &profile.targets, &target);
        let (tx, rx) = watch::channel(distribute.clone());
        drop(profile);

        let new_backend = self.0.clone();
        let span = tracing::debug_span!("router", target = %Param::<profiles::LogicalAddr>::param(&target));
        tokio::spawn(
            async move {
                while profiles.changed().await.is_ok() {
                    let profile = profiles.borrow_and_update();
                    // Update `backends`.
                    // TODO(eliza): can we avoid doing this if the backends haven't
                    // changed at all (e.g., handle updates that only change the
                    // routes?)
                    tracing::debug!(backends = profile.targets.len(), "Updating backends");
                    backends.clear();
                    Self::mk_backends(&mut backends, &new_backend, &profile.targets, &target);

                    // New distribution.
                    // TODO(eliza): if the backends and weights haven't changed, it
                    // would be nice to avoid rebuilding the distributor...
                    let distribute = Self::mk_distribute(&backends, &profile.targets, &target);

                    if tx.send(distribute).is_err() {
                        tracing::debug!(
                            "No services are listening for profile updates, shutting down"
                        );
                        return;
                    }
                }

                tracing::debug!("Profile watch has closed, shutting down");
            }
            .instrument(span),
        );
        Router {
            rx,
            current: distribute,
        }
    }
}

impl<N, U> NewRouter<N, U>
where
    N: NewService<U> + Clone,
    N::Service: Clone + Send + Sync + 'static,
{
    fn mk_backends<T>(
        backends: &mut HashMap<NameAddr, N::Service>,
        new_backend: &N,
        targets: &[profiles::Target],
        target: &T,
    ) where
        U: From<(ConcreteAddr, T)>,
        T: Param<profiles::LogicalAddr> + Clone,
    {
        backends.reserve(targets.len().max(1));
        for t in targets.iter() {
            let addr = t.addr.clone();
            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr.clone(), backend);
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if backends.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            let backend =
                new_backend.new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr, backend);
        }
    }

    fn mk_distribute<T>(
        backends: &HashMap<NameAddr, N::Service>,
        targets: &[profiles::Target],
        target: &T,
    ) -> Distribute<N::Service>
    where
        T: Param<profiles::LogicalAddr>,
    {
        // Create a stack that can distribute requests to the backends.
        let new_distribute = backends
            .iter()
            .map(|(addr, svc)| (addr.clone(), svc.clone()))
            .collect::<NewDistribute<_>>();

        // Build a single distribution service that will be shared across all routes.
        //
        // TODO(ver) Move this into the route stack so that each route's
        // distribution may vary.
        let distribution = if targets.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            Distribution::from(addr)
        } else {
            Distribution::random_available(
                targets
                    .iter()
                    .cloned()
                    .map(|profiles::Target { addr, weight }| (addr, weight)),
            )
            .expect("distribution must be valid")
        };
        new_distribute.new_service(distribution)
    }
}

// === impl Router ===

impl<Req, S: Service<Req> + Clone> Service<Req> for Router<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        // does the current distribution need to be updated?
        if self
            .rx
            .has_changed()
            .expect("router background task terminated unexpectedly!")
        {
            self.current = self.rx.borrow().clone();
        }

        self.current.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let fut = self.current.call(req);
        self.current = self.rx.borrow().clone();
        fut
    }
}
