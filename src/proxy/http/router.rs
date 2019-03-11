use futures::{Future, Poll};
use h2;
use http;
use http::header::CONTENT_LENGTH;
use std::marker::PhantomData;
use std::time::Duration;
use std::fmt;

use never::Never;
use svc;

extern crate linkerd2_router;

pub use self::linkerd2_router::{Recognize, Router};

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Config {
    capacity: usize,
    max_idle_age: Duration,
    proxy_name: &'static str,
}

/// A layer that that builds a routing service.
///
/// A `Rec`-typed `Recognize` instance is used to produce a target for each
/// `Req`-typed request. If the router doesn't already have a `Service` for this
/// target, it uses a `Stk`-typed `Service` stack.
#[derive(Clone, Debug)]
pub struct Layer<Req, Rec: Recognize<Req>> {
    recognize: Rec,
    _p: PhantomData<fn() -> Req>,
}

#[derive(Clone, Debug)]
pub struct Stack<Req, Rec: Recognize<Req>, Stk> {
    recognize: Rec,
    inner: Stk,
    _p: PhantomData<fn() -> Req>,
}

pub struct Service<Req, Rec, Stk>
where
    Rec: Recognize<Req>,
    Stk: svc::Stack<Rec::Target>,
    Stk::Value: svc::Service<Req>,
{
    inner: Router<Req, Rec, Stk>,
}

/// Catches errors from the inner future and maps them to 500 responses.
pub struct ResponseFuture<F> {
    inner: F,
}

// === impl Config ===

impl Config {
    pub fn new(proxy_name: &'static str, capacity: usize, max_idle_age: Duration) -> Self {
        Self {
            proxy_name,
            capacity,
            max_idle_age,
        }
    }
}

// Used for logging contexts
impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.proxy_name.fmt(f)
    }
}

// === impl Layer ===

pub fn layer<Rec, Req>(recognize: Rec) -> Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
{
    Layer {
        recognize,
        _p: PhantomData,
    }
}

impl<Req, Rec, Stk, B> svc::Layer<Config, Rec::Target, Stk> for Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Stk: svc::Stack<Rec::Target> + Clone + Send + Sync + 'static,
    Stk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Stk::Value as svc::Service<Req>>::Error: Into<Error>,
    Stk::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Value = <Stack<Req, Rec, Stk> as svc::Stack<Config>>::Value;
    type Error = <Stack<Req, Rec, Stk> as svc::Stack<Config>>::Error;
    type Stack = Stack<Req, Rec, Stk>;

    fn bind(&self, inner: Stk) -> Self::Stack {
        Stack {
            inner,
            recognize: self.recognize.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<Req, Rec, Stk, B> svc::Stack<Config> for Stack<Req, Rec, Stk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Stk: svc::Stack<Rec::Target> + Clone + Send + Sync + 'static,
    Stk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Stk::Value as svc::Service<Req>>::Error: Into<Error>,
    Stk::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Value = Service<Req, Rec, Stk>;
    type Error = Never;

    fn make(&self, config: &Config) -> Result<Self::Value, Self::Error> {
        let inner = Router::new(
            self.recognize.clone(),
            self.inner.clone(),
            config.capacity,
            config.max_idle_age,
        );
        Ok(Service { inner })
    }
}

fn route_err_to_5xx(e: Error) -> http::StatusCode {
    use self::linkerd2_router::error;

    if let Some(ref r) = e.downcast_ref::<error::MakeRoute>() {
        error!("router error: {:?}", r);
        http::StatusCode::INTERNAL_SERVER_ERROR
    } else if let Some(_) = e.downcast_ref::<error::NotRecognized>() {
        error!("could not recognize request");
        http::StatusCode::INTERNAL_SERVER_ERROR
    } else if let Some(ref c) = e.downcast_ref::<error::NoCapacity>() {
        // TODO For H2 streams, we should probably signal a protocol-level
        // capacity change.
        error!("router at capacity ({})", c.0);
        http::StatusCode::SERVICE_UNAVAILABLE
    } else {
        // Inner
        error!("service error: {}", e);
        http::StatusCode::INTERNAL_SERVER_ERROR
    }
}

// === impl Service ===

impl<Req, Rec, Stk, B> svc::Service<Req> for Service<Req, Rec, Stk>
where
    Rec: Recognize<Req> + Send + Sync + 'static,
    Stk: svc::Stack<Rec::Target> + Send + Sync + 'static,
    Stk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Stk::Value as svc::Service<Req>>::Error: Into<Error>,
    Stk::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Response = <Router<Req, Rec, Stk> as svc::Service<Req>>::Response;
    type Error = h2::Error;
    type Future = ResponseFuture<<Router<Req, Rec, Stk> as svc::Service<Req>>::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| {
            error!("router failed to become ready: {:?}", e);
            h2::Reason::INTERNAL_ERROR.into()
        })
    }

    fn call(&mut self, request: Req) -> Self::Future {
        trace!("routing...");
        let inner = self.inner.call(request);
        ResponseFuture { inner }
    }
}

impl<Req, Rec, Stk> Clone for Service<Req, Rec, Stk>
where
    Rec: Recognize<Req>,
    Stk: svc::Stack<Rec::Target>,
    Stk::Value: svc::Service<Req>,
    Router<Req, Rec, Stk>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// === impl ResponseFuture ===

impl<F, B> Future for ResponseFuture<F>
where
    F: Future<Item = http::Response<B>, Error = Error>,
    B: Default,
{
    type Item = F::Item;
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
