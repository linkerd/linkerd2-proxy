use crate::Service;
use linkerd_error::Error;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait MakeConnection<T> {
    type Connection: AsyncRead + AsyncWrite;
    type Error: Into<Error>;
    type Future: std::future::Future<Output = Result<Self::Connection, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>>;

    fn make_connection(&mut self, t: T) -> Self::Future;
}

impl<T, S, I> MakeConnection<T> for S
where
    S: Service<T, Response = I>,
    S::Error: Into<linkerd_error::Error>,
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    type Connection = I;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Service::poll_ready(self, cx)
    }

    fn make_connection(&mut self, t: T) -> Self::Future {
        Service::call(self, t)
    }
}
