use futures::{try_ready, Future, Poll};
use linkerd2_error::{Error, Never};
use tokio;
use tower::Service;

pub trait Bind {
    type Connection;
    type Listen: Listen<Connection = Self::Connection>;

    fn bind(self) -> std::io::Result<Self::Listen>;
}

pub trait Listen {
    type Connection;
    type Error: Into<Error>;

    fn listen_addr(&self) -> std::net::SocketAddr;

    fn poll_accept(&mut self) -> Poll<Self::Connection, Self::Error>;

    fn serve<A: Accept<Self::Connection>>(
        self,
        accept: A,
    ) -> Serve<Self, A, tokio::executor::DefaultExecutor>
    where
        Self: Sized,
        Serve<Self, A, tokio::executor::DefaultExecutor>: Future,
    {
        Serve {
            listen: self,
            accept,
            executor: tokio::executor::DefaultExecutor::current(),
        }
    }
}

/// Handles an accepted connection.
pub trait Accept<C> {
    type ConnectionError: Into<Error>;
    type ConnectionFuture: Future<Item = (), Error = Self::ConnectionError>;
    type Error: Into<Error>;
    type Future: Future<Item = Self::ConnectionFuture, Error = Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    fn accept(&mut self, connection: C) -> Self::Future;

    fn into_service(self) -> AcceptService<Self>
    where
        Self: Sized,
    {
        AcceptService(self)
    }
}

#[derive(Clone, Debug)]
pub struct AcceptService<S>(S);

impl<C, S, E> Accept<C> for S
where
    E: Into<Error>,
    S: Service<C>,
    S::Response: Future<Item = (), Error = E>,
    S::Error: Into<Error>,
{
    type ConnectionError = E;
    type ConnectionFuture = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Service::poll_ready(self)
    }

    fn accept(&mut self, connection: C) -> Self::Future {
        Service::call(self, connection)
    }
}

impl<C, S: Accept<C>> Service<C> for AcceptService<S> {
    type Response = S::ConnectionFuture;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, connection: C) -> Self::Future {
        self.0.accept(connection)
    }
}

pub struct Serve<L, A, E> {
    listen: L,
    accept: A,
    executor: E,
}

impl<L, A> Serve<L, A, tokio::executor::DefaultExecutor>
where
    L: Listen,
    A: Accept<L::Connection>,
    A::Future: Send + 'static,
{
    pub fn with_executor<E: tokio::executor::Executor>(self, executor: E) -> Serve<L, A, E> {
        Serve {
            listen: self.listen,
            accept: self.accept,
            executor,
        }
    }
}

impl<L, A, E> Future for Serve<L, A, E>
where
    L: Listen,
    A: Accept<L::Connection>,
    A::Error: Into<Error>,
    A::ConnectionFuture: Send + 'static,
    A::ConnectionError: Into<Error>,
    A::Future: Send + 'static,
    E: tokio::executor::Executor,
{
    type Item = Never;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            try_ready!(self.accept.poll_ready().map_err(Into::into));
            let conn = try_ready!(self.listen.poll_accept().map_err(Into::into));
            let accept = self.accept.accept(conn).map_err(|_| {});
            self.executor
                .spawn(Box::new(
                    accept.and_then(|conn_future| conn_future.map_err(|_| {})),
                ))
                .map_err(Error::from)?;
        }
    }
}
