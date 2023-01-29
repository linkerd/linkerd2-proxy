use crate::{
    failfast::{self, FailFast},
    layer::{self, Layer},
    BoxService, ExtractParam, NewService, Service,
};
use linkerd_error::Error;
use std::{fmt, marker::PhantomData, time};
use tower::buffer::Buffer;

/// A param type that configures the capacity of a queue.
#[derive(Clone, Debug)]
pub struct Capacity(pub usize);

/// A param type that configures a queue's failfast timeout.
#[derive(Clone, Debug)]
pub struct Timeout(pub time::Duration);

/// A `NewService` that builds buffered inner services.
pub struct NewQueue<Z, X, Req, N> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req, Z)>,
}

/// A marker type to indicate a queue applies failfast timeouts.
#[derive(Debug)]
pub enum WithTimeout {}

pub type NewQueueWithTimeout<X, Req, N> = NewQueue<WithTimeout, X, Req, N>;

pub type QueueWithTimeout<Req, Rsp, E = Error> =
    failfast::Gate<Buffer<BoxService<Req, Rsp, E>, Req>>;

/// A marker type to indicate a queue does not apply a timeout.
#[derive(Debug)]
pub enum WithoutTimeout {}

pub type NewQueueWithoutTimeout<X, Req, N> = NewQueue<WithoutTimeout, X, Req, N>;

pub type QueueWithoutTimeout<Req, Rsp, E = Error> = Buffer<BoxService<Req, Rsp, E>, Req>;

// === impl NewQueue ===

impl<T, X, Req, N, S> NewService<T> for NewQueueWithTimeout<X, Req, N>
where
    Req: Send + 'static,
    X: ExtractParam<Capacity, T>,
    X: ExtractParam<Timeout, T>,
    N: NewService<T, Service = S>,
    S: Service<Req> + Send + 'static,
    S::Response: 'static,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Service = QueueWithTimeout<Req, S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let Capacity(capacity) = self.extract.extract_param(&target);
        let Timeout(failfast_timeout) = self.extract.extract_param(&target);
        let buf = layer::mk(move |inner| Buffer::new(BoxService::new(inner), capacity));
        let buf = FailFast::layer_gated(failfast_timeout, buf);
        buf.layer(self.inner.new_service(target))
    }
}

impl<T, X, Req, N, S> NewService<T> for NewQueueWithoutTimeout<X, Req, N>
where
    Req: Send + 'static,
    X: ExtractParam<Capacity, T>,
    N: NewService<T, Service = S>,
    S: Service<Req> + Send + 'static,
    S::Response: 'static,
    S::Error: Into<Error> + Send + Sync + 'static,
    S::Future: Send,
{
    type Service = QueueWithoutTimeout<Req, S::Response, S::Error>;

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
    pub fn layer_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<X: Clone, Req, N> NewQueueWithTimeout<X, Req, N> {
    /// Returns a [`Layer`] that constructs new queued service configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>` and .
    /// [`crate::Param`]`<`[`Timeout`]`>`. If the inner service remains
    /// unavailable for a failast timeout, the queue stops admitting new
    /// requests and requests in the queue are failed eagerly.
    #[inline]
    #[must_use]
    pub fn layer_with_timeout_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        Self::layer_via(extract)
    }
}

impl<Req, N> NewQueueWithTimeout<(), Req, N> {
    /// Returns a [`Layer`] that constructs new queued service configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>` and .
    /// [`crate::Param`]`<`[`Timeout`]`>`. If the inner service remains
    /// unavailable for a failast timeout, the queue stops admitting new
    /// requests and requests in the queue are failed eagerly.
    #[inline]
    #[must_use]
    pub fn layer_with_timeout() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_with_timeout_via(())
    }
}

impl<X: Clone, Req, N> NewQueueWithoutTimeout<X, Req, N> {
    /// Returns a [`Layer`] that constructs new queued service configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>`.
    #[inline]
    #[must_use]
    pub fn layer_without_timeout_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        Self::layer_via(extract)
    }
}

impl<Req, N> NewQueueWithoutTimeout<(), Req, N> {
    /// Returns a [`Layer`] that constructs new [`Buffer`]s configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>`.
    #[inline]
    #[must_use]
    pub fn layer_without_timeout() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_via(())
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
