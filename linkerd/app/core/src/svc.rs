// Possibly unused, but useful during development.
#![allow(dead_code)]

pub use crate::proxy::{buffer, ready};
use crate::{cache, proxy::http, trace, Error};
use linkerd2_concurrency_limit as concurrency_limit;
pub use linkerd2_lock as lock;
pub use linkerd2_stack::{
    self as stack, layer, map_response, map_target, new_service, oneshot, pending, per_service,
    NewService,
};
pub use linkerd2_timeout as timeout;
use std::time::Duration;
use tower::layer::util::{Identity, Stack as Pair};
pub use tower::layer::Layer;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};
use tower_spawn_ready::SpawnReadyLayer;

#[derive(Clone, Debug)]
pub struct Layers<L>(L);

#[derive(Clone, Debug)]
pub struct Stack<S>(S);

pub fn layers() -> Layers<Identity> {
    Layers(Identity::new())
}

pub fn stack<S>(inner: S) -> Stack<S> {
    Stack(inner)
}

pub fn proxies() -> Stack<IdentityProxy> {
    Stack(IdentityProxy(()))
}

#[derive(Copy, Clone, Debug)]
pub struct IdentityProxy(());

impl<T> NewService<T> for IdentityProxy {
    type Service = ();
    fn new_service(&self, _: T) -> Self::Service {
        ()
    }
}

impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Layers<Pair<L, pending::Layer>> {
        self.push(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer<Req>(self, bound: usize) -> Layers<Pair<L, buffer::Layer<Req>>>
    where
        Req: Send + 'static,
    {
        self.push(buffer::Layer::new(bound))
    }

    pub fn push_spawn_ready(self) -> Layers<Pair<L, SpawnReadyLayer>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_lock(self) -> Layers<Pair<L, lock::Layer>> {
        self.push(lock::Layer::default())
    }

    pub fn push_concurrency_limit(self, max: usize) -> Layers<Pair<L, concurrency_limit::Layer>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_load_shed(self) -> Layers<Pair<L, load_shed::Layer>> {
        self.push(load_shed::Layer)
    }

    pub fn push_make_ready<Req>(self) -> Layers<Pair<L, ready::Layer<Req>>> {
        self.push(ready::Layer::new())
    }

    pub fn push_ready_timeout(self, timeout: Duration) -> Layers<Pair<L, timeout::ready::Layer>> {
        self.push(timeout::ready::Layer::new(timeout))
    }

    pub fn boxed<A, B>(self) -> Layers<Pair<L, http::boxed::Layer<A, B>>>
    where
        A: 'static,
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
    {
        self.push(http::boxed::Layer::new())
    }

    pub fn push_oneshot(self) -> Layers<Pair<L, oneshot::Layer>> {
        self.push(oneshot::Layer::new())
    }

    pub fn push_per_service<O: Clone>(self, layer: O) -> Layers<Pair<L, per_service::Layer<O>>> {
        self.push(per_service::layer(layer))
    }

    pub fn push_trace<G: Clone>(self, get_span: G) -> Layers<Pair<L, trace::layer::Layer<G>>> {
        self.push(trace::Layer::new(get_span))
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    pub fn push_map_target<M: Clone>(
        self,
        map_target: M,
    ) -> Stack<map_target::MakeMapTarget<S, M>> {
        self.push(map_target::Layer::new(map_target))
    }

    pub fn push_trace<G: Clone>(self, get_span: G) -> Stack<trace::layer::MakeSpan<G, S>> {
        self.push(trace::Layer::new(get_span))
    }

    pub fn push_pending(self) -> Stack<pending::NewPending<S>> {
        self.push(pending::layer())
    }

    pub fn push_make_ready<Req>(self) -> Stack<ready::MakeReady<S, Req>> {
        self.push(ready::Layer::new())
    }

    pub fn push_lock(self) -> Stack<lock::Lock<S>> {
        self.push(lock::Layer::default())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer<Req>(self, bound: usize) -> Stack<buffer::Buffer<S, Req>>
    where
        Req: Send + 'static,
        S: Service<Req> + Send + 'static,
        S::Error: Into<Error> + Send + Sync,
        S::Future: Send + 'static,
    {
        self.push(buffer::Layer::new(bound))
    }

    pub fn push_spawn_ready(self) -> Stack<tower_spawn_ready::MakeSpawnReady<S>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn push_concurrency_limit(
        self,
        max: usize,
    ) -> Stack<concurrency_limit::ConcurrencyLimit<S>> {
        self.push(concurrency_limit::Layer::new(max))
    }

    pub fn push_load_shed(self) -> Stack<load_shed::LoadShed<S>> {
        self.push(load_shed::Layer)
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(tower::timeout::TimeoutLayer::new(timeout))
    }

    pub fn push_ready_timeout(self, timeout: Duration) -> Stack<timeout::ready::TimeoutReady<S>> {
        self.push(layer::mk(|inner| {
            timeout::ready::TimeoutReady::new(inner, timeout)
        }))
    }

    pub fn push_oneshot(self) -> Stack<oneshot::Oneshot<S>> {
        self.push(oneshot::Layer::new())
    }

    pub fn push_per_service<L: Clone>(self, layer: L) -> Stack<per_service::PerService<L, S>> {
        self.push(per_service::layer(layer))
    }

    pub fn push_map_response<R: Clone>(
        self,
        map_response: R,
    ) -> Stack<map_response::MapResponse<S, R>> {
        self.push(map_response::Layer::new(map_response))
    }

    pub fn push_http_insert_target(self) -> Stack<http::insert::target::NewService<S>> {
        self.push(http::insert::target::layer())
    }

    pub fn spawn_cache<T>(
        self,
        capacity: usize,
        max_idle_age: Duration,
    ) -> Stack<cache::Service<T, S>>
    where
        T: Clone + Eq + std::hash::Hash + Send + 'static,
        S: NewService<T> + Send + 'static,
        S::Service: Clone + Send + 'static,
    {
        Stack(
            cache::Layer::new(capacity, max_idle_age)
                .layer(self.0)
                .spawn(),
        )
    }

    pub fn boxed<A, B>(self) -> Stack<http::boxed::BoxedService<A>>
    where
        A: 'static,
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Error> + 'static,
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
    {
        self.push(http::boxed::Layer::new())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new_service<T>(self) -> Self
    where
        S: NewService<T>,
    {
        self
    }

    pub fn check_new_clone_service<T>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Clone,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_service<T>(self) -> Self
    where
        S: Service<T>,
    {
        self
    }

    pub fn check_service_response<T, U>(self) -> Self
    where
        S: Service<T, Response = U>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new_service_routes<T, Req>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Service<Req>,
    {
        self
    }

    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<T, N> NewService<T> for Stack<N>
where
    N: NewService<T>,
{
    type Service = N::Service;

    fn new_service(&self, t: T) -> Self::Service {
        self.0.new_service(t)
    }
}

impl<T, S> Service<T> for Stack<S>
where
    S: Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        self.0.call(t)
    }
}

/// Proivdes a cloneable Layer, unlike tower::load_shed.
pub mod load_shed {
    pub use tower::load_shed::LoadShed;

    #[derive(Copy, Clone, Debug)]
    pub struct Layer;

    impl<S> super::Layer<S> for Layer {
        type Service = LoadShed<S>;

        fn layer(&self, inner: S) -> Self::Service {
            LoadShed::new(inner)
        }
    }
}

pub mod make_response {
    use super::Oneshot;
    use crate::Error;
    use futures::{try_ready, Future, Poll};

    #[derive(Copy, Clone, Debug)]
    pub struct Layer;

    #[derive(Clone, Debug)]
    pub struct MakeResponse<M>(M);

    pub enum ResponseFuture<F, S: tower::Service<()>> {
        Make(F),
        Respond(Oneshot<S, ()>),
    }

    impl<S> super::Layer<S> for Layer {
        type Service = MakeResponse<S>;

        fn layer(&self, inner: S) -> Self::Service {
            MakeResponse(inner)
        }
    }

    impl<T, M> tower::Service<T> for MakeResponse<M>
    where
        M: tower::MakeService<T, ()>,
        M::MakeError: Into<Error>,
        M::Error: Into<Error>,
    {
        type Response = M::Response;
        type Error = Error;
        type Future = ResponseFuture<M::Future, M::Service>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready().map_err(Into::into)
        }

        fn call(&mut self, req: T) -> Self::Future {
            ResponseFuture::Make(self.0.make_service(req))
        }
    }

    impl<F, S> Future for ResponseFuture<F, S>
    where
        F: Future<Item = S>,
        F::Error: Into<Error>,
        S: tower::Service<()>,
        S::Error: Into<Error>,
    {
        type Item = S::Response;
        type Error = Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            loop {
                *self = match self {
                    ResponseFuture::Make(ref mut fut) => {
                        let svc = try_ready!(fut.poll().map_err(Into::into));
                        ResponseFuture::Respond(Oneshot::new(svc, ()))
                    }
                    ResponseFuture::Respond(ref mut fut) => return fut.poll().map_err(Into::into),
                }
            }
        }
    }
}
