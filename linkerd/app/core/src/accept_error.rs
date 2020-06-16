use futures::{future, FutureExt, TryFutureExt};
use linkerd2_error::{Error, Never};
use linkerd2_proxy_core::listen::Accept;
use std::task::{Context, Poll};
use tracing;

pub struct AcceptError<A>(A);

impl<A> AcceptError<A> {
    pub fn new<T>(inner: A) -> Self
    where
        Self: Accept<T> + Sized,
    {
        AcceptError(inner)
    }
}

impl<T, A> tower::Service<T> for AcceptError<A>
where
    A: Accept<T>,
    A::ConnectionError: 'static,
    A::ConnectionFuture: Send + 'static,
    A::Future: Send + 'static,
{
    type Response = future::Either<
        future::OrElse<
            A::ConnectionFuture,
            future::Ready<Result<(), Never>>,
            fn(A::ConnectionError) -> future::Ready<Result<(), Never>>,
        >,
        future::Ready<Result<(), Never>>,
    >;
    type Error = Never;
    type Future = future::Map<
        A::Future,
        fn(Result<A::ConnectionFuture, A::Error>) -> Result<Self::Response, Never>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(futures::ready!(self.0.poll_ready(cx))
            .map_err(Into::into)
            .expect("poll_ready must never fail on accept")))
    }

    fn call(&mut self, sock: T) -> Self::Future {
        self.0.accept(sock).map(|f| match f {
            Ok(connection_future) => Ok(future::Either::Left(connection_future.or_else(|e| {
                let error: Error = e.into();
                tracing::debug!(%error, "Connection failed");
                future::ok(())
            }))),
            Err(e) => {
                let error: Error = e.into();
                tracing::debug!(%error, "Accept failed");
                Ok(future::Either::Right(future::ok(())))
            }
        })
    }
}
