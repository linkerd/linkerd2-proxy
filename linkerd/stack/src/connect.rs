use crate::{layer, Service};
use futures::prelude::*;
use linkerd_error::Error;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

/// A helper `Service` that drops metadata from a `MakeConnection`
#[derive(Clone, Debug)]
pub struct WithoutConnectionMetadata<S>(S);

/// A helper that coerces a `MakeConnection` into a `Service`
#[derive(Clone, Debug)]
pub struct MakeConnectionService<S>(S);

/// A helper trait that models a `Service` that creates client connections.
///
/// Implementers should implement `Service` and not `MakeConnection`. `MakeConnection` should only
/// be used by consumers of these services.
pub trait MakeConnection<T> {
    /// An I/O type that represents a connection to the remote endpoint.
    type Connection: AsyncRead + AsyncWrite;

    /// Metadata associated with the established connection.
    type Metadata;

    type Error: Into<Error>;

    type Future: Future<Output = Result<(Self::Connection, Self::Metadata), Self::Error>>;

    /// Determines whether the connector is ready to establish a connection.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;

    /// Establishes a connection.
    fn connect(&mut self, t: T) -> Self::Future;

    /// Returns a new `Service` that drops the connection metadata from returned values.
    fn without_connection_metadata(self) -> WithoutConnectionMetadata<Self>
    where
        Self: Sized,
    {
        WithoutConnectionMetadata(self)
    }

    /// Coerces a `MakeConnection` into a `Service`.
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
    S::Error: Into<Error>,
    I: AsyncRead + AsyncWrite,
{
    type Connection = I;
    type Metadata = M;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Service::poll_ready(self, cx)
    }

    #[inline]
    fn connect(&mut self, t: T) -> Self::Future {
        Service::call(self, t)
    }
}

// === impl MakeConnectionService ===

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
        self.0.connect(t)
    }
}

// === impl WithoutConnectionMetadata ===

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
        self.0.connect(t).map_ok(|(conn, _)| conn)
    }
}
