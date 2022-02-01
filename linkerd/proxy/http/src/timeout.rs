use linkerd_error::Error;
use linkerd_stack::{layer, MapErr, NewService, Param, Timeout, TimeoutError};
use thiserror::Error;
use tokio::time::Duration;

#[derive(Clone, Debug)]
pub struct ResponseTimeout(pub Option<Duration>);

#[derive(Clone, Debug, Error)]
#[error("HTTP response timeout after {0:?}")]
pub struct ResponseTimeoutError(Duration);

/// An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
#[derive(Clone, Debug)]
pub struct NewTimeout<M> {
    inner: M,
}

impl<N> NewTimeout<N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(|inner| Self { inner })
    }
}

impl<T, M> NewService<T> for NewTimeout<M>
where
    T: Param<ResponseTimeout>,
    M: NewService<T>,
{
    type Service = MapErr<Timeout<M::Service>, fn(Error) -> Error>;

    fn new_service(&self, target: T) -> Self::Service {
        let svc = match target.param() {
            ResponseTimeout(Some(t)) => Timeout::new(self.inner.new_service(target), t),
            ResponseTimeout(None) => Timeout::passthru(self.inner.new_service(target)),
        };
        MapErr::new(svc, |error| {
            if let Some(t) = error.downcast_ref::<TimeoutError>() {
                ResponseTimeoutError(t.duration()).into()
            } else {
                error
            }
        })
    }
}
