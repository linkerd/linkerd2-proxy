use crate::{
    failfast::{self, FailFast},
    layer::{self, Layer},
    BoxService, ExtractParam, NewService, Service,
};
use linkerd_error::Error;
use std::{fmt, marker::PhantomData, time};
use tower::buffer::Buffer;

#[derive(Debug, Copy, Clone)]
pub struct QueueConfig {
    /// The number of requests (or connections, depending on the context) that
    /// may be buffered
    pub capacity: usize,

    /// The maximum amount of time a request may be buffered before failfast
    /// errors are emitted.
    pub failfast_timeout: time::Duration,
}

#[derive(Clone, Debug)]
pub struct Capacity(pub usize);

#[derive(Clone, Debug)]
pub struct FailFastTimeout(pub time::Duration);

pub struct NewQueue<Z, X, Req, N> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req, Z)>,
}

#[derive(Debug)]
pub enum Timeout {}

pub type NewQueueTimeout<X, Req, N> = NewQueue<Timeout, X, Req, N>;

pub type QueueTimeout<Req, Rsp, E = Error> = failfast::Gate<Buffer<BoxService<Req, Rsp, E>, Req>>;

#[derive(Debug)]
pub enum Forever {}

pub type NewQueueForever<X, Req, N> = NewQueue<Forever, X, Req, N>;

pub type QueueForever<Req, Rsp, E = Error> = Buffer<BoxService<Req, Rsp, E>, Req>;

// === impl NewQueue ===

impl<T, X, Req, N, S> NewService<T> for NewQueueTimeout<X, Req, N>
where
    Req: Send + 'static,
    X: ExtractParam<Capacity, T>,
    X: ExtractParam<FailFastTimeout, T>,
    N: NewService<T, Service = S>,
    S: Service<Req> + Send + 'static,
    S::Response: 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Service = QueueTimeout<Req, S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let Capacity(capacity) = self.extract.extract_param(&target);
        let FailFastTimeout(failfast_timeout) = self.extract.extract_param(&target);
        let buf = layer::mk(move |inner| Buffer::new(BoxService::new(inner), capacity));
        let buf = FailFast::layer_gated(failfast_timeout, buf);
        buf.layer(self.inner.new_service(target))
    }
}

impl<T, X, Req, N, S> NewService<T> for NewQueueForever<X, Req, N>
where
    Req: Send + 'static,
    X: ExtractParam<Capacity, T>,
    N: NewService<T, Service = S>,
    S: Service<Req> + Send + 'static,
    S::Response: 'static,
    S::Error: Into<Error> + Send + Sync + 'static,
    S::Future: Send,
{
    type Service = QueueForever<Req, S::Response, S::Error>;

    fn new_service(&self, target: T) -> Self::Service {
        let Capacity(capacity) = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        Buffer::new(BoxService::new(inner), capacity)
    }
}

impl<Z, X: Clone, Req, N> NewQueue<Z, X, Req, N> {
    pub fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _req: PhantomData,
        }
    }

    /// Returns a [`Layer`] that constructs new [`failfast::Gate`]d [`Buffer`]s
    /// using an `X`-typed [`ExtractParam`] implementation to extract
    /// [`QueueConfig`] from a `T`-typed target.
    #[inline]
    #[must_use]
    pub fn layer_with(extract: X) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<Req, N> NewQueueTimeout<(), Req, N> {
    /// Returns a [`Layer`] that constructs new queued service configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>` and .
    /// [`crate::Param`]`<`[`FailFastTimeout`]`>`. If the inner service remains
    /// unavailable for a failast timeout, the queue stops admitting new
    /// requests and requests in the queue are failed eagerly.
    #[inline]
    #[must_use]
    pub fn layer() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_with(())
    }
}

impl<Req, N> NewQueueForever<(), Req, N> {
    /// Returns a [`Layer`] that constructs new [`Buffer`]s configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>`.
    #[inline]
    #[must_use]
    pub fn layer_forever() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_with(())
    }
}

impl<Z, X, Req, N> Clone for NewQueue<Z, X, Req, N>
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

impl<Z, X, Req, N> fmt::Debug for NewQueue<Z, X, Req, N>
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

// === impl QueueConfig ===

impl<T> ExtractParam<Capacity, T> for QueueConfig {
    #[inline]
    fn extract_param(&self, _: &T) -> Capacity {
        Capacity(self.capacity)
    }
}

impl<T> ExtractParam<FailFastTimeout, T> for QueueConfig {
    #[inline]
    fn extract_param(&self, _: &T) -> FailFastTimeout {
        FailFastTimeout(self.failfast_timeout)
    }
}
