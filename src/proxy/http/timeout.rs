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

#[derive(Clone, Debug)]
pub struct Service<S>(Timeout<S>);

/// A marker set in `http::Response::extensions` that *this* process triggered
/// the request timeout.
#[derive(Debug)]
pub struct ProxyTimedOut(());

impl<T, M> svc::Layer<T, T, M> for Layer
where
    M: svc::Stack<T>,
    T: HasTimeout,
{
    type Value = <Stack<M> as svc::Stack<T>>::Value;
    type Error = <Stack<M> as svc::Stack<T>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack { inner }
    }
}

impl<T, M> svc::Stack<T> for Stack<M>
where
    M: svc::Stack<T>,
    T: HasTimeout,
{
    type Value = svc::Either<Service<M::Value>, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(target)?;
        if let Some(timeout) = target.timeout() {
            Ok(svc::Either::A(Service(Timeout::new(inner, timeout))))
        } else {
            Ok(svc::Either::B(inner))
        }
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
