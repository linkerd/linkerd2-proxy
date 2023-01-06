#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::{future, prelude::*};
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, NewService, Oneshot, Service, ServiceExt};
use std::{
    fmt::Debug,
    marker::PhantomData,
    task::{Context, Poll},
};
use tracing::debug;

mod cache;

pub use self::cache::{Cache, NewCache};

pub trait SelectRoute<Req> {
    type Key;
    type Error: Into<Error>;

    /// Given a a request, returns the key matching this request.
    ///
    /// If no route matches the request, this method returns an error.
    fn select(&self, req: &Req) -> Result<Self::Key, Self::Error>;
}

/// A [`NewService`] that builds `Route` services for targets that provide a
/// [`SelectRoute`].
///
/// The selector is built with an `X`-typed [`ExtractParam`] implementation.
#[derive(Debug)]
pub struct NewOneshotRoute<Sel, X, N> {
    extract: X,
    inner: N,
    _marker: PhantomData<fn() -> Sel>,
}

/// Dispatches requests to a new `S`-typed inner service.
///
/// Each request is matched against the route table and routed to a new inner
/// service via a [`Oneshot`].
#[derive(Clone, Debug)]
pub struct OneshotRoute<Sel, N> {
    select: Sel,
    new_route: N,
}

// === impl NewOneshotRoute ===

impl<Sel, X, N> NewOneshotRoute<Sel, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            extract,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<Sel, X: Clone, N> NewOneshotRoute<Sel, X, N> {
    /// Builds a [`layer::Layer`] that produces `NewOneshotRoute`s.
    ///
    /// Targets must produce a `Sel` via the provided `X`-typed
    /// [`ExtractParam`].
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<Sel, N> NewOneshotRoute<Sel, (), N> {
    /// Builds a [`layer::Layer`] that produces `NewOneshotRoute`s.
    ///
    /// Target types must implement `Param<Sel>`.
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<Sel, K, N> NewOneshotRoute<Sel, (), NewCache<K, N>> {
    /// Builds a [`layer::Layer`] that produces `NewOneshotRoute`s using a cache
    /// of inner services.
    ///
    /// Target types must implement `Param<Sel>`.
    pub fn layer_cached() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner: N| Self::new((), NewCache::new(inner)))
    }
}

impl<T, Sel, X, N> NewService<T> for NewOneshotRoute<Sel, X, N>
where
    X: ExtractParam<Sel, T>,
    N: NewService<T>,
{
    type Service = OneshotRoute<Sel, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let select = self.extract.extract_param(&target);
        let new_route = self.inner.new_service(target);
        OneshotRoute { select, new_route }
    }
}

impl<Sel, X: Clone, N: Clone> Clone for NewOneshotRoute<Sel, X, N> {
    fn clone(&self) -> Self {
        Self {
            extract: self.extract.clone(),
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl OneshotRoute ===

impl<Sel, N, S, Req> Service<Req> for OneshotRoute<Sel, N>
where
    Sel: SelectRoute<Req>,
    N: NewService<Sel::Key, Service = S>,
    S: Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<Oneshot<S, Req>, fn(S::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.select.select(&req) {
            Ok(key) => future::Either::Left({
                let route = self.new_route.new_service(key);
                route.oneshot(req).map_err(Into::into)
            }),
            Err(e) => future::Either::Right({
                let error = e.into();
                debug!(%error, "Failed to route request");
                future::err(error)
            }),
        }
    }
}

// === impl SelectRoute ===

impl<T, K, E, F> SelectRoute<T> for F
where
    K: Clone,
    E: std::error::Error + Send + Sync + 'static,
    F: Fn(&T) -> Result<K, E>,
{
    type Key = K;
    type Error = E;

    fn select(&self, t: &T) -> Result<Self::Key, E> {
        (self)(t)
    }
}
