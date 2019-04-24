use std::time::Duration;

use futures::{future, Future, Poll};
use http::{Request, Response, StatusCode};

use svc;
use svc::linkerd2_timeout::{error, Timeout};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Implement on targets to determine if a service has a timeout.
pub trait HasTimeout {
    fn timeout(&self) -> Option<Duration>;
}

/// An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
pub fn layer() -> Layer {
    Layer
}

#[derive(Clone, Debug)]
pub struct Layer;

#[derive(Clone, Debug)]
pub struct Stack<M> {
    inner: M,
}

pub struct MakeFuture<F> {
    inner: F,
    timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct Service<S>(Timeout<S>);

/// A marker set in `http::Response::extensions` that *this* process triggered
/// the request timeout.
#[derive(Debug)]
pub struct ProxyTimedOut(());

impl<M> svc::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack { inner }
    }
}

impl<T, M> svc::Service<T> for Stack<M>
where
    M: svc::Service<T>,
    T: HasTimeout,
{
    type Response = svc::Either<Service<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let timeout = target.timeout();
        let inner = self.inner.call(target);

        MakeFuture { inner, timeout }
    }
}

impl<F: Future> Future for MakeFuture<F> {
    type Item = svc::Either<Service<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());

        let svc = if let Some(timeout) = self.timeout {
            svc::Either::A(Service(Timeout::new(inner, timeout)))
        } else {
            svc::Either::B(inner)
        };
        Ok(svc.into())
    }
}

impl<S, B1, B2> svc::Service<Request<B1>> for Service<S>
where
    S: svc::Service<Request<B1>, Response = Response<B2>>,
    S::Error: Into<Error>,
    B2: Default,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::OrElse<
        <Timeout<S> as svc::Service<Request<B1>>>::Future,
        Result<Response<B2>, Error>,
        fn(Error) -> Result<Response<B2>, Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: Request<B1>) -> Self::Future {
        self.0.call(req).or_else(|err| {
            if let Some(err) = err.downcast_ref::<error::Timedout>() {
                debug!("request timed out after {:?}", err.duration());
                let mut res = Response::default();
                *res.status_mut() = StatusCode::GATEWAY_TIMEOUT;
                res.extensions_mut().insert(ProxyTimedOut(()));
                return Ok(res);
            } else if let Some(err) = err.downcast_ref::<error::Timer>() {
                // These are unexpected, and mean the runtime is in a bad place.
                error!("unexpected runtime timer error: {}", err);
                let mut res = Response::default();
                *res.status_mut() = StatusCode::BAD_GATEWAY;
                return Ok(res);
            }

            // else
            Err(err)
        })
    }
}
