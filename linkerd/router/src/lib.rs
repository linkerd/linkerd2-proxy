#![deny(warnings, rust_2018_idioms)]

use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use linkerd2_stack::NewService;
use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

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
pub struct NewRouter<T, M> {
    new_recgonize: T,
    make_route: M,
}

#[derive(Clone, Debug)]
pub struct Router<T, M> {
    recognize: T,
    make: M,
}

#[derive(Clone, Debug)]
pub struct RecognizeFn<F>(F);

impl<K: Clone> Layer<K> {
    pub fn new(new_recgonize: K) -> Self {
        Self { new_recgonize }
    }
}

impl<K: Clone, M> tower::layer::Layer<M> for Layer<K> {
    type Service = NewRouter<K, M>;

    fn layer(&self, make_route: M) -> Self::Service {
        NewRouter {
            make_route,
            new_recgonize: self.new_recgonize.clone(),
        }
    }
}

impl<T, K, M> NewService<T> for NewRouter<K, M>
where
    K: NewService<T>,
    M: Clone,
{
    type Service = Router<K::Service, M>;

    fn new_service(&self, t: T) -> Self::Service {
        Router {
            recognize: self.new_recgonize.new_service(t),
            make: self.make_route.clone(),
        }
    }
}

impl<U, S, K, M> tower::Service<U> for Router<K, M>
where
    U: std::fmt::Debug,
    K: Recognize<U>,
    K::Key: std::fmt::Debug,
    M: tower::Service<K::Key, Response = S>,
    M::Error: Into<Error>,
    S: tower::Service<U>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<U, M::Future, S>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.make.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, request: U) -> Self::Future {
        let key = self.recognize.recognize(&request);
        trace!(?key, ?request, "Routing");
        ResponseFuture::Make(self.make.call(key), Some(request))
    }
}

pub enum ResponseFuture<Req, M, S>
where
    M: Future<Item = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    Make(M, Option<Req>),
    Respond(Oneshot<S, Req>),
}

impl<Req, M, S> Future for ResponseFuture<Req, M, S>
where
    M: Future<Item = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Item = S::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ResponseFuture::Make(ref mut fut, ref mut req) => {
                    trace!("Making");
                    let service = try_ready!(fut.poll().map_err(Into::into));
                    let req = req.take().expect("polled after ready");
                    ResponseFuture::Respond(service.oneshot(req))
                }
                ResponseFuture::Respond(ref mut future) => {
                    trace!("Responding");
                    return future.poll().map_err(Into::into);
                }
            }
        }
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
