#![deny(warnings, rust_2018_idioms)]

use futures::{ready, TryFuture};
use linkerd2_error::Error;
use linkerd2_stack::NewService;
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.make.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: U) -> Self::Future {
        let key = self.recognize.recognize(&request);
        trace!(?key, ?request, "Routing");
        ResponseFuture {
            state: State::Make(self.make.call(key), Some(request)),
        }
    }
}

#[pin_project]
pub struct ResponseFuture<Req, M, S>
where
    M: TryFuture<Ok = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    #[pin]
    state: State<Req, M, S>,
}

#[pin_project]
enum State<Req, M, S>
where
    M: TryFuture<Ok = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    Make(#[pin] M, Option<Req>),
    Respond(#[pin] Oneshot<S, Req>),
}

impl<Req, M, S> Future for ResponseFuture<Req, M, S>
where
    M: TryFuture<Ok = S>,
    M::Error: Into<Error>,
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Output = Result<S::Response, Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            #[project]
            match this.state.as_mut().project() {
                State::Make(fut, req) => {
                    trace!("Making");
                    let service = ready!(fut.try_poll(cx)).map_err(Into::into)?;
                    let req = req.take().expect("polled after ready");
                    this.state.set(State::Respond(service.oneshot(req)))
                }
                State::Respond(future) => {
                    trace!("Responding");
                    return future.poll(cx).map_err(Into::into);
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
