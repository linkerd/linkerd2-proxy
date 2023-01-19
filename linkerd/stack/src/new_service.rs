use crate::layer;
use crate::FutureService;
use std::marker::PhantomData;
use tower::util::{Oneshot, ServiceExt};

/// Immediately and infalliby creates (usually) a Service.
pub trait NewService<T> {
    type Service;

    fn new_service(&self, target: T) -> Self::Service;
}

/// A Layer that modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FromMakeServiceLayer(());

/// Modifies inner `MakeService`s to be exposd as a `NewService`.
#[derive(Clone, Copy, Debug)]
pub struct FromMakeService<S> {
    make_service: S,
}

#[derive(Clone, Debug)]
pub struct NewCloneService<S>(S);

#[derive(Debug)]
pub struct NewFromTargets<P, N> {
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

/// Builds services by passing `P` typed values to an `N`-typed inner stack.
#[derive(Debug)]
pub struct NewFromTargetsInner<T, P, N> {
    target: T,
    inner: N,
    _marker: PhantomData<fn() -> P>,
}

// === impl NewService ===

impl<F, T, S> NewService<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn new_service(&self, target: T) -> Self::Service {
        (self)(target)
    }
}

// === impl FromMakeService ===

impl<S> FromMakeService<S> {
    pub fn layer() -> impl super::layer::Layer<S, Service = Self> {
        super::layer::mk(|make_service| Self { make_service })
    }
}

impl<T, S> NewService<T> for FromMakeService<S>
where
    S: tower::Service<T> + Clone,
{
    type Service = FutureService<Oneshot<S, T>, S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        FutureService::new(self.make_service.clone().oneshot(target))
    }
}

// === impl NewCloneService ===

impl<N> NewCloneService<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self)
    }
}

impl<S> From<S> for NewCloneService<S> {
    fn from(inner: S) -> Self {
        Self(inner)
    }
}

impl<T, S: Clone> NewService<T> for NewCloneService<S> {
    type Service = S;

    fn new_service(&self, _: T) -> Self::Service {
        self.0.clone()
    }
}

// === impl NewFromTargets ===

impl<P, N> NewFromTargets<P, N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(NewFromTargets::new)
    }
}

impl<T, P, N> NewService<T> for NewFromTargets<P, N>
where
    T: Clone,
    N: NewService<T>,
{
    type Service = NewFromTargetsInner<T, P, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        // Create a `NewService` that is used to process updates to the watched
        // value. This allows inner stacks to, for instance, scope caches to the
        // target.
        let inner = self.inner.new_service(target.clone());

        NewFromTargetsInner {
            target,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<P, N: Clone> Clone for NewFromTargets<P, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl NewFromTargetsInner ===

impl<T, U, P, N> NewService<U> for NewFromTargetsInner<T, P, N>
where
    T: Clone,
    P: From<(U, T)>,
    N: NewService<P>,
{
    type Service = N::Service;

    fn new_service(&self, target: U) -> Self::Service {
        let p = P::from((target, self.target.clone()));
        self.inner.new_service(p)
    }
}

impl<T: Clone, P, N: Clone> Clone for NewFromTargetsInner<T, P, N> {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}
