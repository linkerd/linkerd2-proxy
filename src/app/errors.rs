//! Layer to map HTTP service errors into appropriate `http::Response`s.

use crate::proxy::http::HasH2Reason;
use crate::svc;
use futures::{Future, Poll};
use http::{header, Request, Response, StatusCode, Version};
use linkerd2_error::Error;
use tracing::{debug, error, warn};

/// Layer to map HTTP service errors into appropriate `http::Response`s.
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
pub struct Service<S>(S);

#[derive(Debug)]
pub struct ResponseFuture<F> {
    inner: F,
    is_http2: bool,
}

#[derive(Clone, Debug)]
pub struct StatusError {
    pub status: http::StatusCode,
    pub message: String,
}

impl<M> svc::Layer<M> for Layer {
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack { inner }
    }
}

impl<T, M> svc::Service<T> for Stack<M>
where
    M: svc::Service<T>,
{
    type Response = Service<M::Response>;
    type Error = M::Error;
    type Future = futures::future::Map<M::Future, fn(M::Response) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }
    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(target).map(Service)
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
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: Request<B1>) -> Self::Future {
        let is_http2 = req.version() == Version::HTTP_2;
        let inner = self.0.call(req);
        ResponseFuture { inner, is_http2 }
    }
}

impl<F, B> Future for ResponseFuture<F>
where
    F: Future<Item = Response<B>>,
    F::Error: Into<Error>,
    B: Default,
{
    type Item = Response<B>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll() {
            Ok(ok) => Ok(ok),
            Err(err) => {
                let err = err.into();

                if self.is_http2 {
                    if err.h2_reason().is_some() {
                        debug!("propagating http2 response error: {:?}", err);
                        return Err(err);
                    }
                }

                let response = Response::builder()
                    .status(map_err_to_5xx(err))
                    .header(header::CONTENT_LENGTH, "0")
                    .body(B::default())
                    .expect("app::errors response is valid");

                Ok(response.into())
            }
        }
    }
}

fn map_err_to_5xx(e: Error) -> StatusCode {
    use crate::proxy::buffer;
    use crate::proxy::http::router::error as router;
    use tower::load_shed::error as shed;

    if let Some(ref c) = e.downcast_ref::<router::NoCapacity>() {
        warn!("router at capacity ({})", c.0);
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if let Some(_) = e.downcast_ref::<shed::Overloaded>() {
        warn!("server overloaded, max-in-flight reached");
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if let Some(_) = e.downcast_ref::<buffer::Aborted>() {
        warn!("request aborted because it reached the configured dispatch deadline");
        http::StatusCode::SERVICE_UNAVAILABLE
    } else if let Some(_) = e.downcast_ref::<router::NotRecognized>() {
        error!("could not recognize request");
        http::StatusCode::BAD_GATEWAY
    } else if let Some(err) = e.downcast_ref::<StatusError>() {
        error!("{} {}", err.status, err.message);
        err.status
    } else {
        // we probably should have handled this before?
        error!("unexpected error: {}", e);
        http::StatusCode::BAD_GATEWAY
    }
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for StatusError {}
