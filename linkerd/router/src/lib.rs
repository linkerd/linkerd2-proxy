#![deny(warnings, rust_2018_idioms)]

use linkerd2_stack::NewService;
use std::task::{Context, Poll};
use tower::util::{Oneshot, ServiceExt};

pub trait Recognize<T> {
    type Key: Clone;

    fn recognize(&self, t: &T) -> Self::Key;
}

pub fn recognize<F>(f: F) -> RecognizeFn<F> {
    RecognizeFn(f)
}

#[derive(Clone, Debug)]
pub struct Layer<T> {
    new_recgonize: T,
}

#[derive(Clone, Debug)]
pub struct NewRouter<T, N> {
    new_recgonize: T,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Router<T, N> {
    recognize: T,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct RecognizeFn<F>(F);

impl<K: Clone> Layer<K> {
    pub fn new(new_recgonize: K) -> Self {
        Self { new_recgonize }
    }
}

impl<K: Clone, N> tower::layer::Layer<N> for Layer<K> {
    type Service = NewRouter<K, N>;

    fn layer(&self, inner: N) -> Self::Service {
        NewRouter {
            inner,
            new_recgonize: self.new_recgonize.clone(),
        }
    }
}

impl<T, K, N> NewService<T> for NewRouter<K, N>
where
    K: NewService<T>,
    N: Clone,
{
    type Service = Router<K::Service, N>;

    fn new_service(&mut self, t: T) -> Self::Service {
        Router {
            recognize: self.new_recgonize.new_service(t),
            inner: self.inner.clone(),
        }
    }
}

impl<K, N, S, Req> tower::Service<Req> for Router<K, N>
where
    K: Recognize<Req>,
    N: NewService<K::Key, Service = S>,
    S: tower::Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Oneshot<S, Req>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let key = self.recognize.recognize(&req);
        self.inner.new_service(key).oneshot(req)
    }
}

impl<T, K, F> Recognize<T> for RecognizeFn<F>
where
    K: Clone,
    F: Fn(&T) -> K,
{
    type Key = K;

    fn recognize(&self, t: &T) -> Self::Key {
        (self.0)(t)
    }
}
