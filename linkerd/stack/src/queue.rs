use crate::{
    failfast::{self, FailFast},
    layer::{self, Layer},
    BoxService, CloneParam, ExtractParam, NewService, Service,
};
use linkerd_error::Error;
use std::{fmt, marker::PhantomData, time::Duration};
use tower::buffer::Buffer;

#[derive(Debug, Copy, Clone)]
pub struct QueueConfig {
    /// The number of requests (or connections, depending on the context) that
    /// may be buffered
    pub capacity: usize,

    /// The maximum amount of time a request may be buffered before failfast
    /// errors are emitted.
    pub failfast_timeout: Duration,
}

pub struct NewQueue<X, Req, N> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req)>,
}

pub type Queue<Req, Rsp> = failfast::Gate<Buffer<BoxService<Req, Rsp, Error>, Req>>;

// === impl NewQueue ===

impl<T, X, Req, N> NewService<T> for NewQueue<X, Req, N>
where
    Req: Send + 'static,
    X: ExtractParam<QueueConfig, T>,
    N: NewService<T>,
    N::Service: Service<Req> + Send + 'static,
    <N::Service as Service<Req>>::Future: Send,
    <N::Service as Service<Req>>::Error: Into<Error>,
    <N::Service as Service<Req>>::Response: 'static,
{
    type Service = Queue<Req, <N::Service as Service<Req>>::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let QueueConfig {
            capacity,
            failfast_timeout,
        } = self.extract.extract_param(&target);
        let buf = layer::mk(move |inner| Buffer::new(BoxService::new(inner), capacity));
        let buf = FailFast::layer_gated(failfast_timeout, buf);
        buf.layer(self.inner.new_service(target))
    }
}

impl<X: Clone, Req, N> NewQueue<X, Req, N> {
    /// Returns a [`Layer`] that constructs new [`failfast::Gate`]d [`Buffer`]s
    /// using an `X`-typed [`ExtractParam`] implementation to extract
    /// [`QueueConfig`] from a `T`-typed target.
    #[inline]
    #[must_use]
    pub fn layer_with(extract: X) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _req: PhantomData,
        })
    }
}

impl<Req, N> NewQueue<(), Req, N> {
    /// Returns a [`Layer`] that constructs new [`failfast::Gate`]d [`Buffer`]s
    /// configured by a target that implements
    /// [`crate::Param`]`<`[`QueueConfig`]`>`.
    #[inline]
    #[must_use]
    pub fn layer() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_with(())
    }
}

impl<Req, N> NewQueue<CloneParam<QueueConfig>, Req, N> {
    /// Returns a [`Layer`] that constructs new [`failfast::Gate`]d [`Buffer`]s
    /// using a fixed [`QueueConfig`] regardless of the target.
    #[inline]
    #[must_use]
    pub fn layer_fixed(params: QueueConfig) -> impl Layer<N, Service = Self> + Clone {
        Self::layer_with(CloneParam(params))
    }
}

impl<X, Req, N> Clone for NewQueue<X, Req, N>
where
    X: Clone,
    N: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _req: PhantomData,
        }
    }
}

impl<X, Req, N> fmt::Debug for NewQueue<X, Req, N>
where
    X: fmt::Debug,
    N: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewQueue")
            .field("inner", &self.inner)
            .field("extract", &self.extract)
            .finish()
    }
}
