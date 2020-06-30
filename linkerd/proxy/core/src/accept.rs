use linkerd2_error::Error;
use std::future::Future;
use std::task::{Context, Poll};
use tower::Service;

/// Handles an accepted connection.
pub trait Accept<C> {
    type ConnectionError: Into<Error>;
    type ConnectionFuture: Future<Output = Result<(), Self::ConnectionError>>;
    type Error: Into<Error>;
    type Future: Future<Output = Result<Self::ConnectionFuture, Self::Error>>;

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

impl<C, S, E> Accept<C> for S
where
    E: Into<Error>,
    S: Service<C>,
    S::Response: Future<Output = Result<(), E>>,
    S::Error: Into<Error>,
{
    type ConnectionError = E;
    type ConnectionFuture = S::Response;
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
    type Response = S::ConnectionFuture;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, connection: C) -> Self::Future {
        self.0.accept(connection)
    }
}
