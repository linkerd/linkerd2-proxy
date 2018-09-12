use futures::{Future, Poll};
use h2;
use http;
use http::header::CONTENT_LENGTH;
use std::{fmt, error};
use std::sync::Arc;

use ctx;
use svc::{MakeClient, Service};

extern crate linkerd2_proxy_router;

use self::linkerd2_proxy_router::Error;
pub use self::linkerd2_proxy_router::{Recognize, Router};

pub struct Make<R>
where
    R: Recognize,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
{
    router: Router<R>,
}

pub struct RouteService<R>
where
    R: Recognize,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
{
    inner: Router<R>,
}

/// Catches errors from the inner future and maps them to 500 responses.
pub struct ResponseFuture<R>
where
    R: Recognize,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
{
    inner: <Router<R> as Service>::Future,
}

// ===== impl Make =====

impl<R, A, B> Make<R>
where
    R: Recognize<Request = http::Request<A>, Response = http::Response<B>>,
    R: Send + Sync + 'static,
    R::Error: error::Error + Send + 'static,
    R::RouteError: fmt::Display + Send + 'static,
    A: Send + 'static,
    B: Default + Send + 'static,
{
    pub fn new(router: Router<R>) -> Self {
        Self { router }
    }
}

impl<R> Clone for Make<R>
where
    R: Recognize,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
{
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
        }
    }
}

impl<R, A, B> MakeClient<Arc<ctx::transport::Server>> for Make<R>
where
    R: Recognize<Request = http::Request<A>, Response = http::Response<B>>,
    R: Send + Sync + 'static,
    R::Error: error::Error + Send + 'static,
    R::RouteError: fmt::Display + Send + 'static,
    A: Send + 'static,
    B: Default + Send + 'static,
{
    type Error = ();
    type Client = RouteService<R>;

    fn make_client(&self, _: &Arc<ctx::transport::Server>) -> Result<Self::Client, Self::Error> {
        let inner = self.router.clone();
        Ok(RouteService { inner })
    }
}

fn route_err_to_5xx<E, F>(e: Error<E, F>) -> http::StatusCode
where
    E: fmt::Display,
    F: fmt::Display,
{
    match e {
        Error::Route(r) => {
            error!("router error: {}", r);
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
        Error::Inner(i) => {
            error!("service error: {}", i);
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
        Error::NotRecognized => {
            error!("could not recognize request");
            http::StatusCode::INTERNAL_SERVER_ERROR
        }
        Error::NoCapacity(capacity) => {
            // TODO For H2 streams, we should probably signal a protocol-level
            // capacity change.
            error!("router at capacity ({})", capacity);
            http::StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

// ===== impl RouteService =====

impl<R, B> Service for RouteService<R>
where
    R: Recognize<Response = http::Response<B>>,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
    B: Default,
{
    type Request = <Router<R> as Service>::Request;
    type Response = <Router<R> as Service>::Response;
    type Error = h2::Error;
    type Future = ResponseFuture<R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| {
            error!("router failed to become ready: {}", e);
            h2::Reason::INTERNAL_ERROR.into()
        })
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        let inner = self.inner.call(request);
        ResponseFuture { inner }
    }
}

// ===== impl ResponseFuture =====

impl<R, B> Future for ResponseFuture<R>
where
    R: Recognize<Response = http::Response<B>>,
    R::Error: error::Error,
    R::RouteError: fmt::Display,
    B: Default,
{
    type Item = R::Response;
    type Error = h2::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.inner.poll().or_else(|e| {
            let response = http::Response::builder()
                .status(route_err_to_5xx(e))
                .header(CONTENT_LENGTH, "0")
                .body(B::default())
                .unwrap();

            Ok(response.into())
        })
    }
}
