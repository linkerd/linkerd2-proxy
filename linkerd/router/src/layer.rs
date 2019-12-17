use crate::{Recognize, Router};
use futures::{Future, Poll};
use linkerd2_error::{Error, Never};
use linkerd2_stack::Make;
use std::marker::PhantomData;
use std::time::Duration;
use tracing::info_span;
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
pub struct MakeRouter<Req, Rec: Recognize<Req>, Mk> {
    config: Config,
    recognize: Rec,
    inner: Mk,
    _p: PhantomData<fn() -> Req>,
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
    Mk: Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Service: tower::Service<Req>,
    <Mk::Service as tower::Service<Req>>::Error: Into<Error>,
{
    type Service = MakeRouter<Req, Rec, Mk>;

    fn layer(&self, inner: Mk) -> Self::Service {
        MakeRouter {
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
// === impl MakeRouter ===

impl<Req, Rec, Mk> MakeRouter<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    <Rec as Recognize<Req>>::Target: Send + 'static,
    Mk: Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Service: tower::Service<Req> + Send + 'static,
    <Mk::Service as tower::Service<Req>>::Error: Into<Error>,
{
    pub fn spawn(&self) -> Router<Req, Rec, Mk> {
        let (router, purge) = Router::new(
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
        router
    }
}

impl<Req, Rec, Mk, T> Make<T> for MakeRouter<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    <Rec as Recognize<Req>>::Target: Send + 'static,
    Mk: Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Service: tower::Service<Req> + Send + 'static,
    <Mk::Service as tower::Service<Req>>::Error: Into<Error>,
{
    type Service = Router<Req, Rec, Mk>;

    fn make(&self, _: T) -> Self::Service {
        self.spawn()
    }
}

impl<Req, Rec, Mk, T> tower::Service<T> for MakeRouter<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone + Send + Sync + 'static,
    <Rec as Recognize<Req>>::Target: Send + 'static,
    Mk: Make<Rec::Target> + Clone + Send + Sync + 'static,
    Mk::Service: tower::Service<Req> + Send + 'static,
    <Mk::Service as tower::Service<Req>>::Error: Into<Error>,
{
    type Response = Router<Req, Rec, Mk>;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to make a Router
    }

    fn call(&mut self, _: T) -> Self::Future {
        futures::future::ok(self.spawn())
    }
}

impl<Req, Rec, Mk> Clone for MakeRouter<Req, Rec, Mk>
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
