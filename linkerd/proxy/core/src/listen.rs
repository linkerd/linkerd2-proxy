use linkerd2_error::{Error, Never};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio;
use tower::Service;
use tracing_futures::Instrument;

pub trait Bind {
    type Connection;
    type Listen: Listen<Connection = Self::Connection>;

    fn bind(self) -> std::io::Result<Self::Listen>;
}

pub trait Listen {
    type Connection;
    type Error: Into<Error>;

    fn listen_addr(&self) -> std::net::SocketAddr;

    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<Result<Self::Connection, Self::Error>>;

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
    type Future: Future<Output = Result<(), Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

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

impl<C, S> Accept<C> for S
where
    S: Service<C, Response = ()>,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(self, cx)
    }

    fn accept(&mut self, connection: C) -> Self::Future {
        Service::call(self, connection)
    }
}

impl<C, S: Accept<C>> Service<C> for AcceptService<S> {
    type Response = ();
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, connection: C) -> Self::Future {
        self.0.accept(connection)
    }
}

#[pin_project::pin_project]
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
    type Output = Result<Never, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        loop {
            futures::ready!(this.accept.poll_ready(cx))?;
            let conn = futures::ready!(this.listen.poll_accept(cx).map_err(Into::into))?;
            let accept = (this.accept).accept(conn).in_current_span();
            tokio::spawn(accept);
        }
    }
}
