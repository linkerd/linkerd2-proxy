extern crate linkerd2_router as rt;
pub extern crate linkerd2_stack as stack;
pub extern crate linkerd2_timeout;

pub use self::linkerd2_timeout::stack as timeout;
pub use self::stack::{layer, shared, Layer, LayerExt};
pub use tower::util::{Either, Oneshot};
pub use tower::{service_fn as mk, MakeConnection, MakeService, Service, ServiceExt};

use std::time::Duration;
use tower::builder::ServiceBuilder;
use tower::layer::util::{Identity, Stack};
use tower::limit::concurrency::ConcurrencyLimitLayer;
use tower::load_shed::LoadShedLayer;
use tower::timeout::TimeoutLayer;

use proxy::{buffer, http::fallback, pending};

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
    pub fn buffer_pending<D, Req>(
        self,
        bound: usize,
        d: D,
    ) -> Builder<Stack<pending::Layer, Stack<buffer::Layer<D, Req>, L>>>
    where
        D: buffer::Deadline<Req>,
        Req: Send + 'static,
    {
        self.layer(buffer::layer(bound, d)).layer(pending::layer())
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

    /// Returns a `Layer` that tries to build a service using `self`, falling
    /// back to `other` if `self` fails.
    pub fn fallback_to<B>(self, other: Builder<B>) -> fallback::Layer<L, B> {
        fallback::layer(self, other)
    }

    /// Wrap the service `S` with the layers.
    pub fn service<S>(self, service: S) -> L::Service
    where
        L: Layer<S>,
    {
        self.0.service(service)
    }
}
