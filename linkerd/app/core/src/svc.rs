use crate::proxy::{buffer, http};
use crate::transport::Connect;
use crate::Error;
pub use linkerd2_box as boxed;
use linkerd2_concurrency_limit as concurrency_limit;
pub use linkerd2_stack::{self as stack, fallback, layer, new_service, NewService, Shared};
pub use linkerd2_stack_tracing::{InstrumentMake, InstrumentMakeLayer};
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

pub fn connect(keepalive: Option<Duration>) -> Stack<Connect> {
    Stack(Connect::new(keepalive))
}

// Possibly unused, but useful during development.
#[allow(dead_code)]
impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn push_into_new_service(self) -> Layers<Pair<L, new_service::FromMakeServiceLayer>> {
        self.push(new_service::FromMakeServiceLayer::default())
    }

    /// Buffers requests in an mpsc, spawning the inner service onto a dedicated task.
    pub fn push_buffer<D, Req>(self, bound: usize, d: D) -> Layers<Pair<L, buffer::Layer<D, Req>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.push(buffer::layer(bound, d))
    }

    pub fn push_on_response<U>(self, layer: U) -> Layers<Pair<L, stack::OnResponseLayer<U>>> {
        self.push(stack::OnResponseLayer::new(layer))
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

    pub fn push_instrument<G: Clone>(self, get_span: G) -> Layers<Pair<L, InstrumentMakeLayer<G>>> {
        self.push(InstrumentMakeLayer::new(get_span))
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

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn into_new_service(self) -> Stack<new_service::FromMakeService<S>> {
        self.push(new_service::FromMakeServiceLayer::default())
    }

    /// Buffers requests in an mpsc, spawning the inner service onto a dedicated task.
    pub fn spawn_buffer<D, Req>(self, bound: usize, d: D) -> Stack<buffer::Enqueue<S, D, Req>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
        S: tower::Service<Req> + Send + 'static,
        S::Error: Into<Error>,
        S::Future: Send,
    {
        self.push(buffer::layer(bound, d))
    }

    /// Assuming `S` implements `NewService` or `MakeService`, applies the given
    /// `L`-typed layer on each service produced by `S`.
    pub fn push_on_response<L: Clone>(self, layer: L) -> Stack<stack::OnResponse<L, S>> {
        self.push(stack::OnResponseLayer::new(layer))
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

    pub fn push_map_target<M: Clone>(self, map: M) -> Stack<stack::MapTargetService<S, M>> {
        self.push(stack::MapTargetLayer::new(map))
    }

    pub fn push_map_response<M: Clone>(self, map: M) -> Stack<stack::MapResponse<S, M>> {
        self.push(stack::MapResponseLayer::new(map))
    }

    pub fn instrument<G: Clone>(self, get_span: G) -> Stack<InstrumentMake<G, S>> {
        self.push(InstrumentMakeLayer::new(get_span))
    }

    pub fn instrument_from_target(self) -> Stack<InstrumentMake<(), S>> {
        self.push(InstrumentMakeLayer::from_target())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_new_service<T>(self) -> Self
    where
        S: new_service::NewService<T>,
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

    /// Validates that this stack serves T-typed targets.
    pub fn check_make_service<T, Req>(self) -> Self
    where
        S: MakeService<T, Req>,
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
