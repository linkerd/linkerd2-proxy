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

#[derive(Debug)]
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
    U: From<(ConcreteAddr, T)> + 'static,
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
        let targets = profile
            .targets
            .iter()
            .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
            .collect::<HashMap<_, _>>();
        // TODO(ver) use a different key type that is structured instead of
        // simply a name.
        let mut backends = HashMap::with_capacity(targets.len().max(1));
        self.mk_backends(&mut backends, &targets, &target);

        // Create a stack that can distribute requests to the backends.
        let distribute = Self::mk_distribute(&backends, &targets, &target);
        let (tx, rx) = watch::channel(distribute.clone());
        drop(profile);

        tokio::spawn(
            self.clone()
                .rebuild(tx, backends, targets, target, profiles)
                .in_current_span(),
        );
        Router {
            rx,
            current: distribute,
        }
    }
}

impl<N, U> NewRouter<N, U>
where
    N: NewService<U>,
    N::Service: Clone,
{
    async fn rebuild<T>(
        self,
        tx: watch::Sender<Distribute<N::Service>>,
        mut backends: HashMap<NameAddr, N::Service>,
        mut targets: HashMap<NameAddr, u32>,
        target: T,
        mut profiles: profiles::Receiver,
    ) where
        T: Param<profiles::LogicalAddr> + Clone,
        U: From<(ConcreteAddr, T)>,
    {
        while profiles.changed().await.is_ok() {
            let profile = profiles.borrow_and_update();
            let new_targets = profile
                .targets
                .iter()
                .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
                .collect::<HashMap<_, _>>();

            if new_targets != targets {
                tracing::debug!(backends = new_targets.len(), "Updated backends");

                // Clear out old backends.
                let removed_backends = {
                    let current_backends = backends.len();
                    backends.retain(|addr, _| new_targets.contains_key(addr));
                    current_backends - backends.len()
                };

                // Update `backends`.
                let added_backends = self.mk_backends(&mut backends, &new_targets, &target);

                // Update `distribute`.
                let distribute = Self::mk_distribute(&backends, &new_targets, &target);

                targets = new_targets;

                tracing::debug!(
                    backends.added = added_backends,
                    backends.removed = removed_backends,
                    "Updated backends"
                );

                if tx.send(distribute).is_err() {
                    tracing::debug!("No services are listening for profile updates, shutting down");
                    return;
                }
            } else {
                // Skip updating the backends if the targets and weights are
                // unchanged.
                tracing::debug!("Targets and weights have not changed");
            }
        }
    }

    fn mk_backends<T>(
        &self,
        backends: &mut HashMap<NameAddr, N::Service>,
        targets: &HashMap<NameAddr, u32>,
        target: &T,
    ) -> usize
    where
        U: From<(ConcreteAddr, T)>,
        T: Param<profiles::LogicalAddr> + Clone,
    {
        backends.reserve(targets.len().max(1));
        let mut new_backends = 0;
        for addr in targets.keys() {
            // Skip rebuilding targets we already have a stack for.
            if backends.contains_key(addr) {
                continue;
            }
            let backend = self
                .0
                .new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr.clone(), backend);
            new_backends += 1;
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if backends.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            let backend = self
                .0
                .new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr, backend);
            return 1;
        }

        new_backends
    }

    fn mk_distribute<T>(
        backends: &HashMap<NameAddr, N::Service>,
        targets: &HashMap<NameAddr, u32>,
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
                targets.iter().map(|(addr, weight)| (addr.clone(), *weight)),
            )
            .expect("distribution must be valid")
        };
        new_distribute.new_service(distribution)
    }
}

impl<N: Clone, U> Clone for NewRouter<N, U> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), std::marker::PhantomData)
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
