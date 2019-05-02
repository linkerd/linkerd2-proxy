use futures::Poll;
use http;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use never::Never;

use proxy::Error;
use svc;

pub extern crate linkerd2_router as rt;

use self::rt::Make;
pub use self::rt::{error, Recognize, Router};

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
/// target, it uses a `Mk`-typed `Service` stack.
#[derive(Clone, Debug)]
pub struct Layer<Req, Rec: Recognize<Req>> {
    recognize: Rec,
    _p: PhantomData<fn() -> Req>,
}

#[derive(Debug)]
pub struct Stack<Req, Rec: Recognize<Req>, Mk> {
    recognize: Rec,
    inner: Mk,
    _p: PhantomData<fn() -> Req>,
}

pub struct Service<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: rt::Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    inner: Router<Req, Rec, Mk>,
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

impl<Req, Rec, Mk, B> svc::Layer<Mk> for Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Mk: rt::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Service = Stack<Req, Rec, Mk>;

    fn layer(&self, inner: Mk) -> Self::Service {
        Stack {
            inner,
            recognize: self.recognize.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<Req, Rec, Mk, B> rt::Make<Config> for Stack<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Mk: rt::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Value = Service<Req, Rec, Mk>;
    fn make(&self, config: &Config) -> Self::Value {
        let inner = Router::new(
            self.recognize.clone(),
            self.inner.clone(),
            config.capacity,
            config.max_idle_age,
        );
        Service { inner }
    }
}

impl<Req, Rec, Mk, B> svc::Service<Config> for Stack<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Mk: rt::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Response = Service<Req, Rec, Mk>;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, config: Config) -> Self::Future {
        futures::future::ok(self.make(&config))
    }
}

impl<Req, Rec: Recognize<Req> + Clone, Mk: Clone> Clone for Stack<Req, Rec, Mk> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            recognize: self.recognize.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Service ===

impl<Req, Rec, Mk, B> svc::Service<Req> for Service<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Send + Sync + 'static,
    Mk: rt::Make<Rec::Target> + Send + Sync + 'static,
    Mk::Value: svc::Service<Req, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Response = <Router<Req, Rec, Mk> as svc::Service<Req>>::Response;
    type Error = <Router<Req, Rec, Mk> as svc::Service<Req>>::Error;
    type Future = <Router<Req, Rec, Mk> as svc::Service<Req>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, request: Req) -> Self::Future {
        trace!("routing...");
        self.inner.call(request)
    }
}

impl<Req, Rec, Mk> Clone for Service<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: rt::Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    Router<Req, Rec, Mk>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
