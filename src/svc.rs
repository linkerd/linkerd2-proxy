use crate::proxy::{buffer, pending};
pub use linkerd2_stack::{self as stack, layer, shared, Layer, LayerExt};
pub use linkerd2_timeout::stack as timeout;
use std::time::Duration;
use tower::builder::ServiceBuilder;
use tower::layer::util::{Identity, Stack};
use tower::limit::concurrency::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};
use tower_spawn_ready::SpawnReadyLayer;

#[derive(Clone, Debug)]
pub struct Builder<L>(ServiceBuilder<L>);

pub fn builder() -> Builder<Identity> {
    Builder(ServiceBuilder::new())
}

impl<L> Builder<L> {
    pub fn layer<T>(self, l: T) -> Builder<Stack<T, L>> {
        Builder(self.0.layer(l))
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn pending(self) -> Builder<Stack<pending::Layer, L>> {
        self.layer(pending::layer())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn buffer_pending<D, Req>(
        self,
        bound: usize,
        d: D,
    ) -> Builder<Stack<pending::Layer, Stack<buffer::Layer<D, Req>, L>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.layer(buffer::layer(bound, d)).pending()
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn spawn_ready(self) -> Builder<Stack<SpawnReadyLayer, L>> {
        self.layer(SpawnReadyLayer::new())
    }

    pub fn concurrency_limit(self, max: usize) -> Builder<Stack<ConcurrencyLimitLayer, L>> {
        Builder(self.0.concurrency_limit(max))
    }

    pub fn load_shed(self) -> Builder<Stack<LoadShedLayer, L>> {
        Builder(self.0.load_shed())
    }

    pub fn timeout(self, timeout: Duration) -> Builder<Stack<TimeoutLayer, L>> {
        Builder(self.0.timeout(timeout))
    }

    /// Wrap the service `S` with the layers.
    pub fn service<S>(self, service: S) -> L::Service
    where
        L: Layer<S>,
    {
        self.0.service(service)
    }
}
