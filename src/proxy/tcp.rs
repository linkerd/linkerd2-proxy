use crate::svc::{self, ServiceExt};
use crate::Error;
use futures::Future;
use linkerd2_duplex::Duplex;
use std::fmt;
use tokio::io::{AsyncRead, AsyncWrite};

/// Attempt to proxy the `server_io` stream to a `T`-typed target.
///
/// If the target is not valid, an error is logged and the server stream is
/// dropped.
pub(super) fn forward<I, C, T>(
    server_io: I,
    connect: C,
    target: T,
) -> impl Future<Item = (), Error = Error> + Send + 'static
where
    T: Send + 'static,
    I: AsyncRead + AsyncWrite + fmt::Debug + Send + 'static,
    C: svc::Service<T> + Send + 'static,
    C::Error: Into<Error>,
    C::Future: Send + 'static,
    C::Response: AsyncRead + AsyncWrite + fmt::Debug + Send + 'static,
{
    connect
        .oneshot(target)
        .map_err(Into::into)
        .and_then(move |io| Duplex::new(server_io, io).map_err(Into::into))
}
