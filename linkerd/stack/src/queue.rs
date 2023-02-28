use crate::{
    failfast::FailFast,
    gate,
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

/// A [`NewService`] that wraps inner services in a [`Buffer`] with a failfast
/// [`Timeout`]. When the inner service is unavailable for that timeout, the
/// queue stops processing new requests and all pending requests are failed.
pub struct NewQueue<X, Req, N> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req)>,
}

/// A [`NewService`] that wraps inner services in a [`Buffer`] **without** a
/// failfast timeout. Requests remain queued until the caller gives up waiting
/// or the inner service becomes available.
pub struct NewQueueWithoutTimeout<X, Req, N> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req)>,
}

pub type Queue<Req, Rsp, E = Error> = gate::Gate<Buffer<BoxService<Req, Rsp, E>, Req>>;

pub type QueueWithoutTimeout<Req, Rsp, E = Error> = Buffer<BoxService<Req, Rsp, E>, Req>;

// === impl NewQueue ===

impl<X: Clone, Req, N> NewQueue<X, Req, N> {
    pub fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _req: PhantomData,
        }
    }

    /// Returns a [`Layer`] that constructs new [`gate::Gate`]d [`Buffer`]s
    /// using an `X`-typed [`ExtractParam`] implementation to extract
    /// [`Capacity`] and [`Timeout`] from a `T`-typed target.
    #[inline]
    #[must_use]
    pub fn layer_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<Req, N> NewQueue<(), Req, N> {
    /// Returns a [`Layer`] that constructs new queued service configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>` and .
    /// [`crate::Param`]`<`[`Timeout`]`>`.
    #[inline]
    #[must_use]
    pub fn layer() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, X, Req, N, S> NewService<T> for NewQueue<X, Req, N>
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
    type Service = Queue<Req, S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let Capacity(capacity) = self.extract.extract_param(&target);
        let Timeout(failfast_timeout) = self.extract.extract_param(&target);
        let buf = layer::mk(move |inner| Buffer::new(BoxService::new(inner), capacity));
        let buf = FailFast::layer_gated(failfast_timeout, buf);
        buf.layer(self.inner.new_service(target))
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

// === impl NewQueueWithoutTimeout ===

impl<X: Clone, Req, N> NewQueueWithoutTimeout<X, Req, N> {
    pub fn new(inner: N, extract: X) -> Self {
        Self {
            inner,
            extract,
            _req: PhantomData,
        }
    }

    /// Returns a [`Layer`] that constructs new [`Buffer`]s using an `X`-typed
    /// [`ExtractParam`] implementation to extract [`Capacity`] from a
    /// `T`-typed target.
    #[inline]
    #[must_use]
    pub fn layer_via(extract: X) -> impl Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, extract.clone()))
    }
}

impl<Req, N> NewQueueWithoutTimeout<(), Req, N> {
    /// Returns a [`Layer`] that constructs new [`Buffer`]s configured by a
    /// target that implements [`crate::Param`]`<`[`Capacity`]`>`.
    #[inline]
    #[must_use]
    pub fn layer() -> impl Layer<N, Service = Self> + Clone {
        Self::layer_via(())
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

impl<X, Req, N> Clone for NewQueueWithoutTimeout<X, Req, N>
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

impl<X, Req, N> fmt::Debug for NewQueueWithoutTimeout<X, Req, N>
where
    X: fmt::Debug,
    N: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewQueueWithoutTimeout")
            .field("inner", &self.inner)
            .field("extract", &self.extract)
            .finish()
    }
}
