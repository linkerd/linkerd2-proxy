use crate::proxy::{buffer, pending};
pub use linkerd2_router::Make;
pub use linkerd2_stack::{self as stack, layer, map_target, shared, Layer, LayerExt};
pub use linkerd2_timeout::stack as timeout;
use std::time::Duration;
use tower::builder::ServiceBuilder;
use tower::layer::util::{Identity, Stack as Pair};
use tower::limit::concurrency::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};
use tower_spawn_ready::SpawnReadyLayer;

#[derive(Clone, Debug)]
pub struct Layers<L>(ServiceBuilder<L>);

#[derive(Clone, Debug)]
pub struct Stack<S>(S);

pub fn layers() -> Layers<Identity> {
    Layers(ServiceBuilder::new())
}

pub fn stack<S>(inner: S) -> Stack<S> {
    Stack(inner)
}

impl<L> Layers<L> {
    pub fn and_then<T>(self, l: T) -> Layers<Pair<T, L>> {
        Layers(self.0.layer(l))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn pending(self) -> Layers<Pair<pending::Layer, L>> {
        self.and_then(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn buffer_pending<D, Req>(
        self,
        bound: usize,
        d: D,
    ) -> Layers<Pair<pending::Layer, Pair<buffer::Layer<D, Req>, L>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.and_then(buffer::layer(bound, d)).pending()
    }

    pub fn spawn_ready(self) -> Layers<Pair<SpawnReadyLayer, L>> {
        self.and_then(SpawnReadyLayer::new())
    }

    pub fn into_inner(self) -> L {
        self.0.into_inner()
    }
}

impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_pending(self) -> Stack<pending::MakePending<S>> {
        self.push(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn push_buffer_pending<Req, D>(
        self,
        bound: usize,
        d: D,
    ) -> Stack<buffer::Make<pending::MakePending<S>, D, Req>>
    where
        Req: Send + 'static,
        D: buffer::Deadline<Req>,
    {
        self.push_pending().push(buffer::layer(bound, d))
    }

    pub fn push_concurrency_limit(self, max: usize) -> Stack<tower::limit::ConcurrencyLimit<S>> {
        self.push(ConcurrencyLimitLayer::new(max))
    }

    pub fn push_load_shed(self) -> Stack<tower::load_shed::LoadShed<S>> {
        self.push(LoadShedLayer::new())
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(TimeoutLayer::new(timeout))
    }

    pub fn into_inner(self) -> S {
        self.0
    }
}

// Possibly unused, but useful during development.
#[allow(dead_code)]
impl<S> Stack<S> {
    pub fn push_spawn_ready(self) -> Stack<tower_spawn_ready::MakeSpawnReady<S>> {
        self.push(SpawnReadyLayer::new())
    }

    /// Validates that this stack serves T-typed targets.
    pub fn serves<T>(self) -> Self
    where
        S: Service<T>,
    {
        self
    }

    /// Validates that this stack makes T-typed targets.
    pub fn makes<T>(self) -> Self
    where
        S: Make<T>,
    {
        self
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
