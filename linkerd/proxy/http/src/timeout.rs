use linkerd_error::Error;
use linkerd_stack::{
    layer,
    timeout::{Timeout, TimeoutError, TimeoutFuture},
    ExtractParam, MapErr, NewService, Service,
};
use std::time::Duration;
use thiserror::Error;

/// An HTTP-specific optional timeout layer.
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

/// A Service that applies a timeout extracted from a request's extensions.
///
/// If a `ResponseTimeout`-typed extension is found in the request's extensions,
/// this `Service` wraps the future returned by an inner `Service` in a timeout
/// with that duration. Otherwise, no timeout is applied.
#[derive(Clone, Debug)]
pub struct TimeoutFromRequest<S>(S);

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
        MapErr::new(svc, ResponseTimeoutError::wrap_err)
    }
}

// === impl TimeoutFromRequest ===

impl<S> TimeoutFromRequest<S> {
    pub fn layer() -> impl tower::layer::Layer<S, Service = Self> + Clone {
        layer::mk(Self)
    }
}

impl<S, B> Service<http::Request<B>> for TimeoutFromRequest<S>
where
    S: Service<http::Request<B>>,
    S::Error: Into<Error>,
{
    type Future = futures::future::MapErr<TimeoutFuture<S::Future>, fn(Error) -> Error>;
    type Error = Error;
    type Response = S::Response;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        use futures::TryFutureExt;

        let timeout = req.extensions().get::<ResponseTimeout>().cloned();
        let f = match timeout {
            Some(ResponseTimeout(Some(t))) => {
                tracing::trace!(timeout = ?t, "Using timeout from request extensions");
                TimeoutFuture::Timeout(tokio::time::timeout(t, self.0.call(req)), t)
            }
            _ => TimeoutFuture::Passthru(self.0.call(req)),
        };
        f.map_err(ResponseTimeoutError::wrap_err)
    }
}

// === impl ResponseTimeoutError ===

impl ResponseTimeoutError {
    fn wrap_err(error: Error) -> Error {
        if let Some(t) = error.downcast_ref::<TimeoutError>() {
            ResponseTimeoutError(t.duration()).into()
        } else {
            error
        }
    }
}
