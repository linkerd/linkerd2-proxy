use crate::{Recognize, Router};
use futures::{Future, Poll};
use linkerd2_error::{Error, Never};
use std::marker::PhantomData;
use std::time::Duration;
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Config {
    capacity: usize,
    max_idle_age: Duration,
}

/// A layer that that builds a routing service.
///
/// A `Rec`-typed `Recognize` instance is used to produce a target for each
/// `Req`-typed request. If the router doesn't already have a `Service` for this
/// target, it uses a `Mk`-typed `Service` stack.
#[derive(Debug)]
pub struct Layer<Req, Rec: Recognize<Req>> {
    config: Config,
    recognize: Rec,
    _p: PhantomData<fn() -> Req>,
}

#[derive(Debug)]
pub struct Make<Req, Rec: Recognize<Req>, Mk> {
    config: Config,
    recognize: Rec,
    inner: Mk,
    _p: PhantomData<fn() -> Req>,
}

pub struct Service<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: super::Make<Rec::Target>,
    Mk::Value: tower::Service<Req>,
{
    inner: Router<Req, Rec, Mk>,
}

// === impl Config ===

impl Config {
    pub fn new(capacity: usize, max_idle_age: Duration) -> Self {
        Self {
            capacity,
            max_idle_age,
        }
    }
}

// === impl Layer ===

impl<Req, Rec> Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
{
    pub fn new(config: Config, recognize: Rec) -> Self {
        Self {
            config,
            recognize,
            _p: PhantomData,
        }
    }
}

impl<Req, Rec, Mk> tower::layer::Layer<Mk> for Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    Mk: super::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: tower::Service<Req> + Clone,
    <Mk::Value as tower::Service<Req>>::Error: Into<Error>,
{
    type Service = Make<Req, Rec, Mk>;

    fn layer(&self, inner: Mk) -> Self::Service {
        Make {
            inner,
            config: self.config.clone(),
            recognize: self.recognize.clone(),
            _p: PhantomData,
        }
    }
}

impl<Req, Rec> Clone for Layer<Req, Rec>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self::new(self.config.clone(), self.recognize.clone())
    }
}
// === impl Make ===

impl<Req, Rec, Mk> Make<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    <Rec as Recognize<Req>>::Target: Send + 'static,
    Mk: super::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: tower::Service<Req> + Clone + Send + 'static,
    <Mk::Value as tower::Service<Req>>::Error: Into<Error>,
{
    pub fn spawn(&self) -> Service<Req, Rec, Mk> {
        let (inner, purge) = Router::new(
            self.recognize.clone(),
            self.inner.clone(),
            self.config.capacity,
            self.config.max_idle_age,
        );
        tokio::spawn(
            purge
                .map_err(|e| match e {})
                .instrument(info_span!("router.purge")),
        );
        Service { inner }
    }
}

impl<Req, Rec, Mk, T> tower::Service<T> for Make<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    <Rec as Recognize<Req>>::Target: Send + 'static,
    Mk: super::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: tower::Service<Req> + Clone + Send + 'static,
    <Mk::Value as tower::Service<Req>>::Error: Into<Error>,
{
    type Response = Service<Req, Rec, Mk>;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, _: T) -> Self::Future {
        futures::future::ok(self.spawn())
    }
}

impl<Req, Rec, Mk> Clone for Make<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone,
    Mk: Clone,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            recognize: self.recognize.clone(),
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}
// === impl Service ===

impl<Req, Rec, Mk> tower::Service<Req> for Service<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Send + Sync + 'static,
    Mk: super::Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Value: tower::Service<Req> + Clone,
    <Mk::Value as tower::Service<Req>>::Error: Into<Error>,
{
    type Response = <Router<Req, Rec, Mk> as tower::Service<Req>>::Response;
    type Error = <Router<Req, Rec, Mk> as tower::Service<Req>>::Error;
    type Future = <Router<Req, Rec, Mk> as tower::Service<Req>>::Future;

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
    Mk: super::Make<Rec::Target>,
    Mk::Value: tower::Service<Req>,
    Router<Req, Rec, Mk>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
