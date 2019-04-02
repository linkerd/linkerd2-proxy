use futures::Poll;
use http;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use never::Never;
use svc;

extern crate linkerd2_router;

pub use self::linkerd2_router::{error, Recognize, Router};

// compiler doesn't notice this type is used in where bounds below...
#[allow(unused)]
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
    type Error = <Router<Req, Rec, Stk> as svc::Service<Req>>::Error;
    type Future = <Router<Req, Rec, Stk> as svc::Service<Req>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, request: Req) -> Self::Future {
        trace!("routing...");
        self.inner.call(request)
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
