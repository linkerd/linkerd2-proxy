use futures::{try_ready, Future, Poll};
use linkerd2_error::{Error, Never};
use tokio;
use tower::Service;

pub trait Listen {
    type Connection;
    type Error: Into<Error>;

    fn poll_accept(&mut self) -> Poll<Self::Connection, Self::Error>;

    fn serve<A: Accept<Self::Connection>>(self, accept: A) -> Serve<Self, A>
    where
        Self: Sized,
        Serve<Self, A>: Future,
    {
        Serve {
            listen: self,
            accept,
        }
    }
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

pub struct Serve<L, A> {
    listen: L,
    accept: A,
}

impl<L, A> Future for Serve<L, A>
where
    L: Listen,
    A: Accept<L::Connection, Error = Never>,
    A::Future: Send + 'static,
{
    type Item = Never;
    type Error = L::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            try_ready!(self.accept.poll_ready().map_err(|e| match e {}));
            let conn = try_ready!(self.listen.poll_accept());

            tokio::spawn(self.accept.accept(conn).map_err(|e| match e {}));
        }
    }
}
