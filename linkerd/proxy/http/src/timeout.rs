use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, MapErr, NewService, Timeout, TimeoutError};
use std::time::Duration;
use thiserror::Error;

/// DEPRECATED: An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
#[derive(Clone, Debug)]
pub struct NewTimeout<X, N> {
    inner: N,
    extract: X,
}

/// Param type configuring a timeout for HTTP responses.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResponseTimeout(pub Option<Duration>);

#[derive(Clone, Debug, Error)]
#[error("HTTP response timeout after {0:?}")]
pub struct ResponseTimeoutError(Duration);

// === impl NewTimeout ===

impl<X: Clone, N> NewTimeout<X, N> {
    pub fn layer_via(extract: X) -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
        })
    }
}

impl<N> NewTimeout<(), N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, X, N> NewService<T> for NewTimeout<X, N>
where
    X: ExtractParam<ResponseTimeout, T>,
    N: NewService<T>,
{
    type Service = MapErr<fn(Error) -> Error, Timeout<N::Service>>;

    fn new_service(&self, target: T) -> Self::Service {
        let svc = match self.extract.extract_param(&target) {
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
