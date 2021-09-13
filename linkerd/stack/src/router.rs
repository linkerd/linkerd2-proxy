use crate::{layer, NewService};
use futures::{future, TryFutureExt};
use linkerd_error::Error;
use std::task::{Context, Poll};
use tower::util::{Oneshot, ServiceExt};

/// Determines the router key for each `T`-typed target.
pub trait RecognizeRoute<T> {
    type Key: Clone;

    fn recognize(&self, t: &T) -> Result<Self::Key, Error>;
}

#[derive(Clone, Debug)]
pub struct NewRouter<T, N> {
    new_recognize: T,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Router<T, N> {
    recognize: T,
    inner: N,
}

impl<K, N> NewRouter<K, N> {
    fn new(new_recognize: K, inner: N) -> Self {
        Self {
            new_recognize,
            inner,
        }
    }

    /// Creates a layer that creates that produces Routers.
    ///
    /// The provided `new_recognize` is expected to implement a `NewService` that
    /// produces `Recognize` implementations.
    pub fn layer(new_recognize: K) -> impl layer::Layer<N, Service = Self> + Clone
    where
        K: Clone,
    {
        layer::mk(move |inner| Self::new(new_recognize.clone(), inner))
    }
}

impl<T, K, N> NewService<T> for NewRouter<K, N>
where
    K: NewService<T>,
    N: Clone,
{
    type Service = Router<K::Service, N>;

    fn new_service(&self, t: T) -> Self::Service {
        Router {
            recognize: self.new_recognize.new_service(t),
            inner: self.inner.clone(),
        }
    }
}

impl<K, N, S, Req> tower::Service<Req> for Router<K, N>
where
    K: RecognizeRoute<Req>,
    N: NewService<K::Key, Service = S>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<Oneshot<S, Req>, Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.recognize.recognize(&req) {
            Ok(key) => {
                let svc = self.inner.new_service(key);
                future::Either::Left(svc.oneshot(req).err_into::<Error>())
            }
            Err(e) => future::Either::Right(future::err(e)),
        }
    }
}

impl<T, K, F> RecognizeRoute<T> for F
where
    K: Clone,
    F: Fn(&T) -> Result<K, Error>,
{
    type Key = K;

    fn recognize(&self, t: &T) -> Result<Self::Key, Error> {
        (self)(t)
    }
}
