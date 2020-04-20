use futures_03::{future, FutureExt};
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
{
    type Response = ();
    type Error = Never;
    type Future = future::Map<A::Future, fn(Result<(), A::Error>) -> Result<(), Never>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(futures_03::ready!(self.0.poll_ready(cx))
            .map_err(Into::into)
            .expect("poll_ready must never fail on accept")))
    }

    fn call(&mut self, sock: T) -> Self::Future {
        self.0.accept(sock).map(|v| {
            if let Err(e) = v {
                let error: Error = e.into();
                tracing::debug!(%error, "Connection failed");
            }
            Ok(())
        })
    }
}
