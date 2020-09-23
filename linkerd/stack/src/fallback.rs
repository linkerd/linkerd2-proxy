//! A middleware that may retry a request in a fallback service.

use super::NewService;
use futures::TryFuture;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::util::{Either, Oneshot, ServiceExt};

/// A Layer that augments the underlying service with a fallback service.
///
/// If the future returned by the primary service fails with an error matching a
/// given predicate, the fallback service is called. The result is returned in an `Either`.
#[derive(Clone, Debug)]
pub struct FallbackLayer<F, P = fn(&Error) -> bool> {
    fallback: F,
    predicate: P,
}

#[derive(Clone, Debug)]
pub struct Fallback<I, F, P = fn(&Error) -> bool> {
    inner: I,
    fallback: F,
    predicate: P,
}

pub struct ReadyFallback<N, T, A, B, P> {
    inner: Either<A, B>,
    fallback: Option<(N, T)>,
    predicate: P,
}

#[pin_project]
pub struct MakeFuture<A, B, P> {
    #[pin]
    state: State<A, B, P>,
}

#[pin_project(project = StateProj)]
enum State<A, B, P> {
    A {
        #[pin]
        primary: A,
        fallback: Option<B>,
        predicate: P,
    },
    B(#[pin] B),
}

// === impl FallbackLayer ===

impl<B> FallbackLayer<B> {
    pub fn new(fallback: B) -> Self {
        let predicate: fn(&Error) -> bool = |_| true;
        Self {
            fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> FallbackLayer<B, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        FallbackLayer {
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> FallbackLayer<B>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| is_error::<E>(e.as_ref()))
    }
}

fn is_error<E>(err: &(dyn std::error::Error + 'static)) -> bool
where
    E: std::error::Error + 'static,
{
    if err.is::<E>() {
        return true;
    }

    err.source().map(is_error::<E>).unwrap_or(false)
}

impl<A, B, P> tower::layer::Layer<A> for FallbackLayer<B, P>
where
    B: Clone,
    P: Clone,
{
    type Service = Fallback<A, B, P>;

    fn layer(&self, inner: A) -> Self::Service {
        Self::Service {
            inner,
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
        }
    }
}

// === impl Fallback ===

impl<A, N, T, P> NewService<T> for Fallback<A, N, P>
where
    T: Clone,
    A: NewService<T>,
    N: NewService<T> + Clone,
    P: Fn(&Error) -> bool,
    P: Clone,
{
    type Service = ReadyFallback<N, T, A::Service, N::Service, P>;

    fn new_service(&mut self, target: T) -> Self::Service {
        ReadyFallback {
            inner: Either::A(self.inner.new_service(target.clone())),
            fallback: Some((self.fallback.clone(), target)),
            predicate: self.predicate.clone(),
        }
    }
}

impl<A, B, P, T> tower::Service<T> for Fallback<A, B, P>
where
    T: Clone,
    A: tower::Service<T>,
    A::Error: Into<Error>,
    B: tower::Service<T> + Clone,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
    P: Clone,
{
    type Response = Either<A::Response, B::Response>;
    type Error = Error;
    type Future = MakeFuture<A::Future, Oneshot<B, T>, P>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        MakeFuture {
            state: State::A {
                primary: self.inner.call(target.clone()),
                fallback: Some(self.fallback.clone().oneshot(target)),
                predicate: self.predicate.clone(),
            },
        }
    }
}

// === impl MakeFuture ===

impl<A, B, P> Future for MakeFuture<A, B, P>
where
    A: TryFuture,
    A::Error: Into<Error>,
    B: TryFuture,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Output = Result<Either<A::Ok, B::Ok>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.state.as_mut().project() {
                StateProj::A {
                    primary,
                    fallback,
                    predicate,
                } => match futures::ready!(primary.try_poll(cx)) {
                    Ok(ok) => return Poll::Ready(Ok(Either::A(ok))),
                    Err(e) => {
                        let error = e.into();
                        if !(predicate)(&error) {
                            return Poll::Ready(Err(error));
                        }
                        let fallback = fallback.take().unwrap();
                        this.state.set(State::B(fallback));
                    }
                },
                StateProj::B(b) => {
                    return b
                        .try_poll(cx)
                        .map(|ok| ok.map(Either::B))
                        .map_err(Into::into);
                }
            };
        }
    }
}

// === impl ReadyFallback ===

impl<N, T, A, B, P, Req> tower::Service<Req> for ReadyFallback<N, T, A, B, P>
where
    A: tower::Service<Req>,
    A::Error: Into<Error>,
    N: NewService<T, Service = B>,
    B: tower::Service<Req, Response = A::Response>,
    B::Error: Into<Error>,
    P: Fn(&Error) -> bool,
{
    type Response = A::Response;
    type Error = Error;
    type Future = Either<A::Future, B::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Fallback may only be set until the initial state is polled to ready.
        match self.fallback {
            None => self.inner.poll_ready(cx),
            ref mut fallback => {
                debug_assert!(matches!(self.inner, Either::A(_)));
                match futures::ready!(self.inner.poll_ready(cx)) {
                    Ok(()) => {
                        // If the primary service becomes ready, drop the
                        // fallback.
                        drop(fallback.take());
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => {
                        // If the initial readiness failed in an expected way,
                        // use the fallback service.
                        if (self.predicate)(&e) {
                            let (mut new, target) = fallback.take().expect("illegal state");
                            self.inner = Either::B(new.new_service(target));
                            self.inner.poll_ready(cx)
                        } else {
                            Poll::Ready(Err(e))
                        }
                    }
                }
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}
