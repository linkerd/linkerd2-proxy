use crate::NewService;
use futures::future;
use linkerd_error::Error;
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct Fail<U, E> {
    _marker: PhantomData<fn() -> Result<U, E>>,
}

impl<T, U, E> NewService<T> for Fail<U, E> {
    type Service = Self;

    #[inline]
    fn new_service(&self, _: T) -> Self::Service {
        *self
    }
}

impl<T, U, E> tower::Service<T> for Fail<U, E>
where
    E: Default + Into<Error>,
{
    type Response = U;
    type Error = Error;
    type Future = future::Ready<Result<U, Error>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, _: T) -> Self::Future {
        future::err(E::default().into())
    }
}

impl<U, E> Default for Fail<U, E> {
    fn default() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<U, E> Clone for Fail<U, E> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

impl<U, E> Copy for Fail<U, E> {}
