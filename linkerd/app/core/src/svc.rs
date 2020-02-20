use crate::proxy::{buffer, http, pending};
use crate::Error;
pub use linkerd2_box as boxed;
use linkerd2_concurrency_limit as concurrency_limit;
pub use linkerd2_fallback as fallback;
pub use linkerd2_router::Make;
pub use linkerd2_stack::{self as stack, layer, map_target, LayerExt, Shared};
pub use linkerd2_timeout::stack as timeout;
use std::time::Duration;
use tower::layer::util::{Identity, Stack as Pair};
pub use tower::layer::Layer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
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

// Possibly unused, but useful during development.
#[allow(dead_code)]
impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Layers<Pair<L, pending::Layer>> {
        self.push(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer_pending<D, Req>(
        self,
        bound: usize,
        d: D,
    ) -> Layers<Pair<Pair<L, pending::Layer>, buffer::Layer<D, Req>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.push_pending().push(buffer::layer(bound, d))
    }

    pub fn push_per_make<T: Clone>(self, layer: T) -> Layers<Pair<L, stack::per_make::Layer<T>>> {
        self.push(stack::per_make::layer(layer))
    }

    pub fn push_spawn_ready(self) -> Layers<Pair<L, SpawnReadyLayer>> {
        self.push(SpawnReadyLayer::new())
    }

    pub fn boxed<A, B>(self) -> Layers<Pair<L, boxed::Layer<A, B>>>
    where
        A: 'static,
        B: 'static,
    {
        self.push(boxed::Layer::new())
    }

    pub fn box_http_request<B>(self) -> Layers<Pair<L, http::boxed::request::Layer<B>>>
    where
        B: hyper::body::Payload + 'static,
    {
        self.push(http::boxed::request::Layer::new())
    }

    pub fn box_http_response(self) -> Layers<Pair<L, http::boxed::response::Layer>> {
        self.push(http::boxed::response::Layer::new())
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

// Possibly unused, but useful during development.
#[allow(dead_code)]
impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Stack<pending::MakePending<S>> {
        self.push(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer_pending<D, Req>(
        self,
        bound: usize,
        d: D,
    ) -> Stack<buffer::Make<pending::MakePending<S>, D, Req>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.push_pending().push(buffer::layer(bound, d))
    }

    pub fn push_per_make<L: Clone>(self, layer: L) -> Stack<stack::per_make::PerMake<L, S>> {
        self.push(stack::per_make::layer(layer))
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

    pub fn push_load_shed(self) -> Stack<tower::load_shed::LoadShed<S>> {
        self.push(LoadShedLayer::new())
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(TimeoutLayer::new(timeout))
    }

    pub fn push_fallback<F: Clone>(self, fallback: F) -> Stack<fallback::Fallback<S, F>> {
        self.push(fallback::FallbackLayer::new(fallback))
    }

    pub fn push_fallback_with_predicate<F, P>(
        self,
        fallback: F,
        predicate: P,
    ) -> Stack<fallback::Fallback<S, F, P>>
    where
        F: Clone,
        P: Fn(&Error) -> bool + Clone,
    {
        self.push(fallback::FallbackLayer::new(fallback).with_predicate(predicate))
    }

    pub fn boxed<A>(self) -> Stack<boxed::BoxService<A, S::Response>>
    where
        A: 'static,
        S: tower::Service<A> + Send + 'static,
        S::Response: 'static,
        S::Future: Send + 'static,
        S::Error: Into<Error> + 'static,
    {
        self.push(boxed::Layer::new())
    }

    pub fn box_http_request<B>(self) -> Stack<http::boxed::BoxRequest<S, B>>
    where
        B: hyper::body::Payload<Data = http::boxed::Data, Error = Error> + 'static,
        S: tower::Service<http::Request<http::boxed::Payload>>,
    {
        self.push(http::boxed::request::Layer::new())
    }

    pub fn box_http_response(self) -> Stack<http::boxed::BoxResponse<S>> {
        self.push(http::boxed::response::Layer::new())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn makes<T>(self) -> Self
    where
        S: Make<T>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn serves<T>(self) -> Self
    where
        S: Service<T>,
    {
        self
    }

    pub fn into_inner(self) -> S {
        self.0
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
