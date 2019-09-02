//! A middleware that wraps `Resolutions`, modifying their endpoint type.

use futures::{Future, Poll};
use linkerd2_proxy_core::{resolve, Error};

pub trait CheckTarget<T> {
    type Error: Into<Error>;

    fn check_target(&self, target: &T) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug)]
pub struct Resolve<C, R> {
    check: C,
    resolve: R,
}

#[derive(Debug)]
pub enum ResolveFuture<F> {
    Future(F),
    Rejected(Option<Error>),
}

// === impl Resolve ===

impl<C, R> Resolve<C, R> {
    pub fn new<T>(check: C, resolve: R) -> Self
    where
        Self: resolve::Resolve<T>,
    {
        Self { check, resolve }
    }
}

impl<T, C, R> tower::Service<T> for Resolve<C, R>
where
    R: resolve::Resolve<T>,
    R::Error: Into<Error>,
    C: CheckTarget<T>,
{
    type Response = R::Resolution;
    type Error = Error;
    type Future = ResolveFuture<R::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        if let Err(e) = self.check.check_target(&target) {
            return ResolveFuture::Rejected(Some(e.into()));
        }

        ResolveFuture::Future(self.resolve.resolve(target))
    }
}

// === impl ResolveFuture ===

impl<F> Future for ResolveFuture<F>
where
    F: Future,
    F::Item: resolve::Resolution,
    F::Error: Into<Error>,
{
    type Item = F::Item;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResolveFuture::Rejected(ref mut e) => Err(e.take().unwrap()),
            ResolveFuture::Future(ref mut f) => f.poll().map_err(Into::into),
        }
    }
}
