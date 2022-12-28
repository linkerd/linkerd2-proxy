use futures::prelude::*;
use linkerd_app_core::{
    profiles,
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewService, Param, Service},
    NameAddr,
};
use std::{
    collections::HashMap,
    task::{Context, Poll},
};
use tokio::sync::watch;

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
    T: Param<profiles::LogicalAddr> + Param<profiles::Receiver> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U> + Clone,
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
        for t in profile.targets.iter() {
            let addr = t.addr.clone();
            let backend = self
                .0
                .new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr.clone(), backend);
        }
        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if backends.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            let backend = self
                .0
                .new_service(U::from((ConcreteAddr(addr.clone()), target.clone())));
            backends.insert(addr, backend);
        }

        // Create a stack that can distribute requests to the backends.
        let new_distribute = backends
            .iter()
            .map(|(addr, svc)| (addr.clone(), svc.clone()))
            .collect::<NewDistribute<_>>();

        // Build a single distribution service that will be shared across all routes.
        //
        // TODO(ver) Move this into the route stack so that each route's
        // distribution may vary.
        let distribution = if profile.targets.is_empty() {
            let profiles::LogicalAddr(addr) = target.param();
            Distribution::from(addr)
        } else {
            Distribution::random_available(
                profile
                    .targets
                    .iter()
                    .cloned()
                    .map(|profiles::Target { addr, weight }| (addr, weight)),
            )
            .expect("distribution must be valid")
        };
        let distribute = new_distribute.new_service(distribution);

        let (tx, rx) = watch::channel(distribute.clone());
        drop(profile);

        tokio::spawn(async move {
            let mut profiles = profiles::ReceiverStream::from(profiles);
            while let Some(_profile) = profiles.next().await {
                // Update `backends`.
                // New distribution.
                // New routes.
                // Publish new shared state.
                todo!();
            }
            drop(backends);
            drop(tx);
        });

        Router {
            rx,
            current: distribute,
        }
    }
}

// === impl Router ===

impl<Req, S: Service<Req> + Clone> Service<Req> for Router<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.current.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let fut = self.current.call(req);
        self.current = self.rx.borrow().clone();
        fut
    }
}
