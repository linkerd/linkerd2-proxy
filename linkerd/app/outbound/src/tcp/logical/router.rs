use linkerd_app_core::{
    profiles::{self, Profile},
    proxy::api_resolve::ConcreteAddr,
    svc::{layer, NewService, Param, Service},
    NameAddr,
};
use std::{
    collections::HashMap,
    marker::PhantomData,
    task::{Context, Poll},
};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Debug)]
pub struct NewRouter<N, U>(N, std::marker::PhantomData<fn(U)>);

#[derive(Clone, Debug)]
pub struct Router<S> {
    rx: watch::Receiver<Distribute<S>>,
    current: Distribute<S>,
}

// TODO(ver) use a different key type that is structured instead of simply a
// name.
type Distribution = linkerd_distribute::Distribution<NameAddr>;
type NewDistribute<S> = linkerd_distribute::NewDistribute<NameAddr, S>;
type Distribute<S> = linkerd_distribute::Distribute<NameAddr, S>;

struct State<T, U, N, S> {
    target: T,

    new_backend: N,
    backends: HashMap<NameAddr, S>,

    _marker: PhantomData<fn(U)>,
}

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
        let mut profiles: profiles::Receiver = target.param();

        // Spawn a background task that updates the the distribution for the
        // router.
        let mut state = State::new(target, self.0.clone());
        let distribute = state
            .update(&*profiles.borrow_and_update())
            .expect("initial update must produce a new state");
        let (tx, rx) = watch::channel(distribute.clone());
        tokio::spawn(
            state
                .run(profiles, tx)
                .instrument(tracing::debug_span!("tcprouter")),
        );

        Router {
            rx,
            current: distribute,
        }
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
        if self
            .rx
            .has_changed()
            .expect("router background task terminated unexpectedly!")
        {
            self.current = self.rx.borrow_and_update().clone();
        }

        self.current.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.current.call(req)
    }
}

// === impl State ===

impl<T, U, N> State<T, U, N, N::Service>
where
    T: Param<profiles::LogicalAddr> + Clone,
    U: From<(ConcreteAddr, T)>,
    N: NewService<U>,
    N::Service: Clone,
{
    fn new(target: T, new_backend: N) -> Self {
        Self {
            target,
            new_backend,
            backends: HashMap::default(),
            _marker: PhantomData,
        }
    }

    async fn run(
        mut self,
        mut profiles: profiles::Receiver,
        tx: watch::Sender<Distribute<N::Service>>,
    ) {
        while profiles.changed().await.is_ok() {
            let profile = profiles.borrow_and_update();
            if let Some(svc) = self.update(&profile) {
                tracing::debug!("Publishing updated state");
                if tx.send(svc).is_err() {
                    return;
                }
            }
        }
    }

    fn update(&mut self, profile: &Profile) -> Option<Distribute<N::Service>> {
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

    fn update_backends<V>(&mut self, targets: &HashMap<NameAddr, V>) -> bool {
        let removed = {
            let init = self.backends.len();
            self.backends.retain(|addr, _| targets.contains_key(addr));
            init - self.backends.len()
        };

        if targets
            .iter()
            .all(|(addr, _)| self.backends.contains_key(addr))
        {
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
