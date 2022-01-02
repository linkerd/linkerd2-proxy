use crate::{layer, Service};
use futures::prelude::*;
use linkerd_error::Error;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Clone, Debug)]
pub struct WithoutConnectionMetadata<S>(S);

#[derive(Clone, Debug)]
pub struct MakeConnectionService<S>(S);

pub trait MakeConnection<T> {
    type Connection: AsyncRead + AsyncWrite;
    type Metadata;
    type Error: Into<Error>;
    type Future: std::future::Future<
        Output = Result<(Self::Connection, Self::Metadata), Self::Error>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    fn make_connection(&mut self, t: T) -> Self::Future;

    fn without_connection_metadata(self) -> WithoutConnectionMetadata<Self>
    where
        Self: Sized,
    {
        WithoutConnectionMetadata(self)
    }

    fn into_service(self) -> MakeConnectionService<Self>
    where
        Self: Sized,
    {
        MakeConnectionService(self)
    }
}

impl<T, S, I, M> MakeConnection<T> for S
where
    S: Service<T, Response = (I, M)>,
    S::Error: Into<linkerd_error::Error>,
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    type Connection = I;
    type Metadata = M;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(self, cx)
    }

    fn make_connection(&mut self, t: T) -> Self::Future {
        Service::call(self, t)
    }
}

impl<T, S> Service<T> for MakeConnectionService<S>
where
    S: MakeConnection<T>,
{
    type Response = (S::Connection, S::Metadata);
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, t: T) -> Self::Future {
        self.0.make_connection(t)
    }
}

impl<S> WithoutConnectionMetadata<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(WithoutConnectionMetadata)
    }
}

impl<T, S> Service<T> for WithoutConnectionMetadata<S>
where
    S: MakeConnection<T>,
{
    type Response = S::Connection;
    type Error = S::Error;
    type Future =
        futures::future::MapOk<S::Future, fn((S::Connection, S::Metadata)) -> S::Connection>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, t: T) -> Self::Future {
        self.0.make_connection(t).map_ok(|(conn, _)| conn)
    }
}
