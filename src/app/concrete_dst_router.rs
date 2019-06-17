use futures::Poll;
use http;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use never::Never;

use svc;
use super::dst::DstAddr;
use proxy::http::settings;
use proxy::http::profiles::ConcreteDst;

extern crate linkerd2_router as rt;

pub use self::rt::{error, Router};

#[allow(dead_code)]
type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Config {
    capacity: usize,
    max_idle_age: Duration,
    proxy_name: &'static str,
}

/// A layer that that builds a service that routes on the concrete DstAddr of
/// a request.  The concrete DstAddr is the ConcreteDst in the request's
/// extension metadata which is set by the profile router.  If no ConcreteDst
/// has been set on the request, the original DstAddr target for this service
/// is used as the concrete DstAddr.
#[derive(Debug)]
pub struct Layer<A> {
    config: Config,
    _p: PhantomData<fn() -> A>,
}

#[derive(Debug)]
pub struct Stack<Mk, A> {
    config: Config,
    inner: Mk,
    _p: PhantomData<fn() -> A>,
}

pub struct Recognize {
    orig_target: DstAddr,
}

pub struct Service<Mk, A>
where
    Mk: rt::Make<DstAddr>,
    Mk::Value: svc::Service<http::Request<A>>,
{
    inner: Router<http::Request<A>, Recognize, Mk>,
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

pub fn layer<Req>(config: Config) -> Layer<Req>
{
    Layer {
        config,
        _p: PhantomData,
    }
}

impl<Mk, A, B> svc::Layer<Mk> for Layer<http::Request<A>>
where
    Mk: rt::Make<DstAddr> + Clone + Send + Sync + 'static,
    Mk::Value: svc::Service<http::Request<A>, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<http::Request<A>>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Service = Stack<Mk, A>;

    fn layer(&self, inner: Mk) -> Self::Service {
        Stack {
            inner,
            config: self.config.clone(),
            _p: PhantomData,
        }
    }
}

impl<Req> Clone for Layer<Req>
{
    fn clone(&self) -> Self {
        layer(self.config.clone())
    }
}

// === impl Recognize ===

impl<A> self::rt::Recognize<http::Request<A>> for Recognize {

    type Target = DstAddr;

    fn recognize(&self, req: &http::Request<A>) -> Option<Self::Target> {
        if let Some(addr) = req.extensions().get::<ConcreteDst>().cloned() {
            debug!("outbound concrete dst={:?}", addr.0);
            let settings = settings::Settings::from_request(req);
            Some(DstAddr::outbound(addr.0.into(), settings))
        } else {
            Some(self.orig_target.clone())
        }
    }
}


// === impl Stack

impl<Mk, A, B> svc::Service<DstAddr> for Stack<Mk, A>
where
    Mk: rt::Make<DstAddr> + Clone + Send + Sync + 'static,
    Mk::Value: svc::Service<http::Request<A>, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<http::Request<A>>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Response = Service<Mk, A>;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, orig_target: DstAddr) -> Self::Future {
        let recognize = Recognize { orig_target };
        let inner = Router::new(
            recognize,
            self.inner.clone(),
            self.config.capacity,
            self.config.max_idle_age,
        );

        futures::future::ok(Service { inner })
    }
}

impl<Mk, A> Clone for Stack<Mk, A>
where
    Mk: Clone,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}
// === impl Service ===

impl<Mk, A, B> svc::Service<http::Request<A>> for Service<Mk, A>
where
    Mk: rt::Make<DstAddr> + Send + Sync + 'static,
    Mk::Value: svc::Service<http::Request<A>, Response = http::Response<B>> + Clone,
    <Mk::Value as svc::Service<http::Request<A>>>::Error: Into<Error>,
    B: Default + Send + 'static,
{
    type Response = <Router<http::Request<A>, Recognize, Mk> as svc::Service<http::Request<A>>>::Response;
    type Error = <Router<http::Request<A>, Recognize, Mk> as svc::Service<http::Request<A>>>::Error;
    type Future = <Router<http::Request<A>, Recognize, Mk> as svc::Service<http::Request<A>>>::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, request: http::Request<A>) -> Self::Future {
        trace!("concrete dst routing...");
        self.inner.call(request)
    }
}

impl<Mk, A> Clone for Service<Mk, A>
where
    Mk: rt::Make<DstAddr>,
    Mk::Value: svc::Service<http::Request<A>>,
    Router<http::Request<A>, Recognize, Mk>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
