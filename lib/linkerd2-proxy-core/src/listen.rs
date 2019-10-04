use futures::{Future, Poll};
use linkerd2_error::Error;
use tower::Service;

pub trait Listen {
    type Connection;
    type Error: Into<Error>;

    fn poll_accept(&mut self) -> Poll<Self::Connection, Self::Error>;
}

/// Handles an accepted connection.
pub trait Accept<C> {
    type Error: Into<Error>;
    type Future: Future<Item = (), Error = Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    fn accept(&mut self, connection: C) -> Self::Future;
}

impl<C, S> Accept<C> for S
where
    S: Service<C, Response = ()>,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Service::poll_ready(self)
    }

    #[inline]
    fn accept(&mut self, connection: C) -> Self::Future {
        Service::call(self, connection)
    }
}
