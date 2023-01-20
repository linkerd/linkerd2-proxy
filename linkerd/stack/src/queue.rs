use crate::{
    failfast::{self, FailFast},
    layer::{self, Layer},
    BoxService, ExtractParam, NewService, Param, Service,
};
use linkerd_error::Error;
use std::{fmt, marker::PhantomData, time::Duration};
use tower::buffer::Buffer;

#[derive(Debug, Copy, Clone)]
pub struct QueueParams {
    /// The number of requests (or connections, depending on the context) that
    /// may be buffered
    pub capacity: usize,

    /// The maximum amount of time a request may be buffered before failfast
    /// errors are emitted.
    pub failfast_timeout: Duration,
}

pub struct NewQueue<N, Req, X = ()> {
    inner: N,
    extract: X,
    _req: PhantomData<fn(Req)>,
}

#[derive(Debug, Copy, Clone)]
pub struct FixedParams(QueueParams);

pub type Queue<Req, Rsp> = failfast::Gate<Buffer<BoxService<Req, Rsp, Error>, Req>>;

// === impl NewQueue ===

impl<T, N, X, Req> NewService<T> for NewQueue<N, Req, X>
where
    Req: Send + 'static,
    X: ExtractParam<QueueParams, T>,
    N: NewService<T>,
    N::Service: Service<Req> + Send + 'static,
    <N::Service as Service<Req>>::Future: Send,
    <N::Service as Service<Req>>::Error: Into<Error>,
    <N::Service as Service<Req>>::Response: 'static,
{
    type Service = Queue<Req, <N::Service as Service<Req>>::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let QueueParams {
            capacity,
            failfast_timeout,
        } = self.extract.extract_param(&target);
        let buf = layer::mk(move |inner| Buffer::new(BoxService::new(inner), capacity));
        let buf = FailFast::layer_gated(failfast_timeout, buf);
        buf.layer(self.inner.new_service(target))
    }
}

impl<T: Param<QueueParams>, Req> NewQueue<T, Req> {
    /// Returns a [`Layer`] that constructs new [`Queue`]s configured by a
    /// `T`-typed target that implements [`Param`]`<`[`QueueParams`]`>`.
    #[inline]
    #[must_use]
    pub fn layer() -> impl Layer<T, Service = Self> + Clone {
        Self::layer_with(())
    }
}

impl<T, Req> NewQueue<T, Req, FixedParams> {
    /// Returns a [`Layer`] that constructs new [`Queue`]s using a fixed set of
    /// [`QueueParams`] regardless of the target.
    #[inline]
    #[must_use]
    pub fn layer_fixed(params: QueueParams) -> impl Layer<T, Service = Self> + Clone {
        Self::layer_with(FixedParams(params))
    }
}

impl<T, Req, X> NewQueue<T, Req, X>
where
    X: ExtractParam<QueueParams, T> + Clone,
{
    /// Returns a [`Layer`] that constructs new [`Queue`]s using an `X`-typed
    /// [`ExtractParam`] implementation to extract [`QueueParams`] from a
    /// `T`-typed target.
    #[inline]
    #[must_use]
    pub fn layer_with(extract: X) -> impl Layer<T, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _req: PhantomData,
        })
    }
}

impl<N, Req, X> Clone for NewQueue<N, Req, X>
where
    N: Clone,
    X: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            extract: self.extract.clone(),
            _req: PhantomData,
        }
    }
}

impl<N, Req, X> fmt::Debug for NewQueue<N, Req, X>
where
    N: fmt::Debug,
    X: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewQueue")
            .field("inner", &self.inner)
            .field("extract", &self.extract)
            .finish()
    }
}

// === impl ExtractParams ===

impl<T> ExtractParam<QueueParams, T> for FixedParams {
    fn extract_param(&self, _: &T) -> QueueParams {
        self.0
    }
}
