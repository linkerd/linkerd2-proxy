use linkerd_app_core::{
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewService, NewSpawnWatch, Param, UpdateWatch},
    NameAddr,
};
use std::{collections::HashMap, marker::PhantomData};

#[derive(Debug)]
pub struct NewRoute<U, N> {
    inner: N,
    _marker: std::marker::PhantomData<fn(U)>,
}

// TODO(ver) use a different key type that is structured instead of simply a
// name.
type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;

struct Update<T, U, N, S> {
    target: T,

    new_backend: N,
    backends: HashMap<NameAddr, S>,

    _marker: PhantomData<fn(U)>,
}

// === impl NewRoute ===

impl<U, N> NewRoute<U, N>
where
    N: NewService<U>,
{
    pub fn layer() -> impl layer::Layer<N, Service = NewSpawnWatch<Profile, Self>> + Clone {
        layer::mk(|inner| {
            NewSpawnWatch::new(Self {
                inner,
                _marker: PhantomData,
            })
        })
    }
}

impl<T, U, N> NewService<T> for NewRoute<U, N>
where
    N: NewService<U> + Clone,
{
    type Service = Update<T, U, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        Update {
            target,
            new_backend: self.inner.clone(),
            backends: HashMap::default(),
            _marker: self._marker,
        }
    }
}

impl<U, N: Clone> Clone for NewRoute<U, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Update ===

impl<T, U, N> UpdateWatch<Profile> for Update<T, U, N, N::Service>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
{
    type Service = Distribute<N::Service>;

    fn update(&mut self, profile: &Profile) -> Option<Self::Service> {
        let targets = profile
            .targets
            .iter()
            .map(|profiles::Target { addr, weight }| (addr.clone(), *weight))
            .collect::<HashMap<_, _>>();

        if self.update_backends(&targets) {
            let new_distribute: NewDistribute<N::Service> = self.backends.clone().into();
            let distribution = if targets.is_empty() {
                let profiles::LogicalAddr(addr) = self.target.param();
                Distribution::from(addr)
            } else {
                Distribution::random_available(
                    targets.iter().map(|(addr, weight)| (addr.clone(), *weight)),
                )
                .expect("distribution must be valid")
            };
            Some(new_distribute.new_service(distribution))
        } else {
            None
        }
    }
}

impl<T, U, N> Update<T, U, N, N::Service>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
{
    fn update_backends<V>(&mut self, targets: &HashMap<NameAddr, V>) -> bool {
        // Drop all backends that are no longer in the set of targets.
        let removed = {
            let init = self.backends.len();
            self.backends.retain(|addr, _| targets.contains_key(addr));
            init - self.backends.len()
        };

        // If there aren't more targets in backends, then there is no more work
        // to be done.
        if !targets.is_empty() && targets.len() == self.backends.len() {
            return removed > 0;
        }

        self.backends.reserve(targets.len().max(1));
        for addr in targets.keys() {
            // Skip rebuilding targets we already have a stack for.
            if self.backends.contains_key(addr) {
                continue;
            }

            let backend = self
                .new_backend
                .new_service(U::from((ConcreteAddr(addr.clone()), self.target.clone())));
            self.backends.insert(addr.clone(), backend);
        }

        // TODO(ver) we should make it a requirement of the provider that there
        // is always at least one backend.
        if self.backends.is_empty() {
            let profiles::LogicalAddr(addr) = self.target.param();
            let backend = self
                .new_backend
                .new_service(U::from((ConcreteAddr(addr.clone()), self.target.clone())));
            self.backends.insert(addr, backend);
        }

        true
    }
}
