use futures::{Future, Poll};
use linkerd2_error::{Error, Never};
use linkerd2_proxy_core::listen::Accept;
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
    A::ConnectionFuture: Send + 'static,
    A::Future: Send + 'static,
{
    type Response = Box<dyn Future<Item = (), Error = Never> + Send + 'static>;
    type Error = A::Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(self
            .0
            .poll_ready()
            .map_err(Into::into)
            .expect("poll_ready must never fail on accept"))
    }

    fn call(&mut self, sock: T) -> Self::Future {
        Box::new(self.0.accept(sock).map(|connection_future| {
            Box::new(connection_future.then(|v| {
                if let Err(e) = v {
                    let error: Error = e.into();
                    tracing::debug!(%error, "Connection failed");
                }
                Ok(())
            })) as Self::Response
        })) as Self::Future
    }
}
