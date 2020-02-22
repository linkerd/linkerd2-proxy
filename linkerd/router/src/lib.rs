#![deny(warnings, rust_2018_idioms)]

mod cache;
pub mod error;
pub mod layer;
mod purge;

use self::cache::Cache;
pub use self::layer::{Config, Layer};
pub use self::purge::Purge;
use futures::{Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_lock::Lock;
use linkerd2_stack::NewService;
use std::hash::Hash;
use std::time::Duration;
pub use tower_load_shed::LoadShed;
use tracing::{debug, trace};

/// Routes requests based on a configurable `Key`.
pub struct Router<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req>,
{
    inner: Inner<Req, Rec, N>,
    _hangup: purge::Handle,
}

/// Provides a strategy for routing a Request to a Service.
///
/// Implementors must provide a `Key` type that identifies each unique route. The
/// `recognize()` method is used to determine the target for a given request. This target is
/// used to look up a route in a cache (i.e. in `Router`), or can be passed to
/// `bind_service` to instantiate the identified route.
pub trait Recognize<Request> {
    /// Identifies a Route.
    type Target: Clone + Eq + Hash;

    /// Determines the target for a route to handle the given request.
    fn recognize(&self, request: &Request) -> Option<Self::Target>;
}

/// A map of known routes and services used when creating a fixed router.
#[derive(Clone, Debug)]
pub struct FixedNewService<T: Clone + Eq + Hash, Svc>(IndexMap<T, Svc>);

pub struct ResponseFuture<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req>,
    <N::Service as tower::Service<Req>>::Error: Into<error::Error>,
{
    state: State<Req, Rec, N>,
}

struct Inner<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req>,
{
    recognize: Rec,
    new_service: N,
    cache: Lock<Cache<Rec::Target, LoadShed<N::Service>>>,
}

enum State<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req>,
    <N::Service as tower::Service<Req>>::Error: Into<error::Error>,
{
    Acquire {
        request: Option<Req>,
        target: Option<Rec::Target>,
        new_service: Option<N>,
        cache: Lock<Cache<Rec::Target, LoadShed<N::Service>>>,
    },
    Call(Option<Req>, Option<LoadShed<N::Service>>),
    Respond(<LoadShed<N::Service> as tower::Service<Req>>::Future),
    Error(Option<error::Error>),
}

// ===== impl Recognize =====

impl<R, T, F> Recognize<R> for F
where
    T: Clone + Eq + Hash,
    F: Fn(&R) -> Option<T>,
{
    type Target = T;

    fn recognize(&self, request: &R) -> Option<T> {
        (self)(request)
    }
}

// ===== impl FixedNewService =====

impl<T, Svc> NewService<T> for FixedNewService<T, Svc>
where
    T: Clone + Eq + Hash,
    Svc: Clone,
{
    type Service = Svc;

    fn new_service(&self, target: T) -> Self::Service {
        self.0
            .get(&target)
            .cloned()
            .expect("target not found in fixed router")
    }
}

// ===== impl Router =====

impl<Req, Rec, N> Router<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    <N as NewService<Rec::Target>>::Service: Clone,
    N::Service: tower::Service<Req>,
{
    pub fn new(
        recognize: Rec,
        new_service: N,
        capacity: usize,
        max_idle_age: Duration,
    ) -> (Self, Purge<Rec::Target, LoadShed<N::Service>>) {
        let cache = Lock::new(Cache::new(capacity, max_idle_age));
        let (purge, _hangup) = Purge::new(cache.clone());
        let router = Self {
            _hangup,
            inner: Inner {
                recognize,
                new_service,
                cache,
            },
        };

        (router, purge)
    }
}

impl<Req, Rec, Svc> Router<Req, Rec, FixedNewService<Rec::Target, Svc>>
where
    Rec: Recognize<Req>,
    Svc: tower::Service<Req> + Clone,
{
    /// A router that is created with a fixed set of known routes.
    ///
    /// `recognize` will only produce targets that exist in `routes`. We never
    /// want to expire routes from the fixed set; the explicit drop of the
    /// daemon task ensures eviction is never performed and `max_idle_age` is
    /// ignored.
    pub fn new_fixed(recognize: Rec, routes: IndexMap<Rec::Target, Svc>) -> Self {
        let capacity = routes.len();
        let (router, _) = Self::new(
            recognize,
            FixedNewService(routes),
            capacity,
            Duration::from_secs(1),
        );

        router
    }

    pub fn into_new_service(self) -> IndexMap<Rec::Target, Svc> {
        self.inner.new_service.0
    }
}

impl<Req, Rec, N> tower::Service<Req> for Router<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target> + Clone,
    N::Service: tower::Service<Req> + Clone,
    <N::Service as tower::Service<Req>>::Error: Into<error::Error>,
{
    type Response = <N::Service as tower::Service<Req>>::Response;
    type Error = error::Error;
    type Future = ResponseFuture<Req, Rec, N>;

    /// Always ready to serve.
    ///
    /// Graceful backpressure is **not** supported at this level, since each request may
    /// be routed to different resources. Instead, requests should be issued and each
    /// route should support a queue of requests.
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    /// Routes the request through an underlying service.
    ///
    /// The response fails when the request cannot be routed.
    fn call(&mut self, request: Req) -> Self::Future {
        let target = match self.inner.recognize.recognize(&request) {
            Some(target) => target,
            None => return ResponseFuture::not_recognized(),
        };

        ResponseFuture::new(
            request,
            target,
            self.inner.new_service.clone(),
            self.inner.cache.clone(),
        )
    }
}

impl<Req, Rec, N> Clone for Router<Req, Rec, N>
where
    Rec: Recognize<Req> + Clone,
    N: NewService<Rec::Target> + Clone,
    N::Service: tower::Service<Req>,
{
    fn clone(&self) -> Self {
        Router {
            inner: self.inner.clone(),
            _hangup: self._hangup.clone(),
        }
    }
}

// ===== impl ResponseFuture =====

impl<Req, Rec, N> ResponseFuture<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req>,
    <N::Service as tower::Service<Req>>::Error: Into<error::Error>,
{
    fn new(
        request: Req,
        target: Rec::Target,
        new_service: N,
        cache: Lock<Cache<Rec::Target, LoadShed<N::Service>>>,
    ) -> Self {
        ResponseFuture {
            state: State::Acquire {
                request: Some(request),
                target: Some(target),
                new_service: Some(new_service),
                cache: cache,
            },
        }
    }

    fn error(err: error::Error) -> Self {
        ResponseFuture {
            state: State::Error(Some(err)),
        }
    }

    fn not_recognized() -> Self {
        Self::error(error::NotRecognized.into())
    }
}

impl<Req, Rec, N> Future for ResponseFuture<Req, Rec, N>
where
    Rec: Recognize<Req>,
    N: NewService<Rec::Target>,
    N::Service: tower::Service<Req> + Clone,
    <N::Service as tower::Service<Req>>::Error: Into<error::Error>,
{
    type Item = <LoadShed<N::Service> as tower::Service<Req>>::Response;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use tower::Service;

        loop {
            self.state = match self.state {
                State::Acquire {
                    ref mut request,
                    ref mut target,
                    ref mut new_service,
                    ref mut cache,
                } => {
                    // Aquire the lock for the router cache
                    let mut cache = match cache.poll_acquire() {
                        Async::Ready(aquired) => aquired,
                        Async::NotReady => return Ok(Async::NotReady),
                    };

                    let request = request.take().expect("polled after ready");
                    let target = target.take().expect("polled after ready");

                    // If the target is already cached, route the request to
                    // the service; otherwise, try to insert it
                    if let Some(service) = cache.access(&target) {
                        trace!("target already cached");
                        State::Call(Some(request), Some(service))
                    } else {
                        debug!("target not cached");

                        // Ensure that there is capacity for a new slot
                        if !cache.can_insert() {
                            debug!("not enough capacity to insert target into cache");
                            return Err(error::NoCapacity(cache.capacity()).into());
                        }

                        // Make a new service for the target
                        let new_service = new_service.take().expect("polled after ready");
                        let service = LoadShed::new(new_service.new_service(target.clone()));

                        debug!("inserting new target into cache");
                        cache.insert(target, service.clone());
                        State::Call(Some(request), Some(service))
                    }
                }
                State::Call(ref mut request, ref mut service) => {
                    let mut service = service.take().expect("polled after ready");

                    assert!(
                        service.poll_ready()?.is_ready(),
                        "load shedding services must always be ready"
                    );

                    let request = request.take().expect("polled after ready");
                    State::Respond(service.call(request))
                }
                State::Respond(ref mut fut) => return fut.poll().map_err(Into::into),
                State::Error(ref mut err) => return Err(err.take().expect("polled after ready")),
            };
        }
    }
}

// ===== impl Inner =====

impl<Req, Rec, N> Clone for Inner<Req, Rec, N>
where
    Rec: Recognize<Req> + Clone,
    N: NewService<Rec::Target> + Clone,
    N::Service: tower::Service<Req>,
{
    fn clone(&self) -> Self {
        Inner {
            recognize: self.recognize.clone(),
            new_service: self.new_service.clone(),
            cache: self.cache.clone(),
        }
    }
}

#[cfg(test)]
mod test_util {
    use futures::{future, Async, Poll};
    use linkerd2_stack::NewService;
    use std::cell::Cell;
    use std::fmt;
    use std::rc::Rc;
    use tower::Service;

    #[derive(Clone)]
    pub struct Recognize;

    #[derive(Clone, Debug)]
    pub struct MultiplyAndAssign(Rc<Cell<usize>>, bool);

    #[derive(Debug, PartialEq)]
    pub enum MulError {
        AtMax,
        Overflow,
    }

    #[derive(Debug)]
    pub enum Request {
        NotRecognized,
        Recognized(usize),
    }

    // ===== impl Recognize =====

    impl super::Recognize<Request> for Recognize {
        type Target = usize;

        fn recognize(&self, request: &Request) -> Option<Self::Target> {
            match *request {
                Request::NotRecognized => None,
                Request::Recognized(n) => Some(n),
            }
        }
    }

    impl NewService<usize> for Recognize {
        type Service = MultiplyAndAssign;

        fn new_service(&self, _: usize) -> Self::Service {
            MultiplyAndAssign::default()
        }
    }

    // ===== impl MultiplyAndAssign =====

    impl MultiplyAndAssign {
        pub fn new(n: usize) -> Self {
            MultiplyAndAssign(Rc::new(Cell::new(n)), true)
        }

        pub fn never_ready() -> Self {
            MultiplyAndAssign(Rc::new(Cell::new(0)), false)
        }
    }

    impl Default for MultiplyAndAssign {
        fn default() -> Self {
            MultiplyAndAssign::new(1)
        }
    }

    impl NewService<usize> for MultiplyAndAssign {
        type Service = MultiplyAndAssign;

        fn new_service(&self, _: usize) -> Self::Service {
            // Don't use a clone, so that they don't affect the original Stack...
            MultiplyAndAssign(Rc::new(Cell::new(self.0.get())), self.1)
        }
    }

    impl Service<Request> for MultiplyAndAssign {
        type Response = usize;
        type Error = MulError;
        type Future = future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            if !self.1 {
                return Ok(Async::NotReady);
            }

            if self.0.get() < ::std::usize::MAX - 1 {
                Ok(().into())
            } else {
                Err(MulError::AtMax)
            }
        }

        fn call(&mut self, request: Request) -> Self::Future {
            let n = match request {
                Request::NotRecognized => unreachable!(),
                Request::Recognized(n) => n,
            };
            let a = self.0.get();
            match a.checked_mul(n) {
                Some(x) => {
                    self.0.set(x);
                    future::ok(x)
                }
                None => future::err(MulError::Overflow),
            }
        }
    }

    impl From<usize> for Request {
        fn from(n: usize) -> Request {
            Request::Recognized(n)
        }
    }

    impl fmt::Display for MulError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{:?}", self)
        }
    }

    impl std::error::Error for MulError {}
}

#[cfg(test)]
mod tests {
    use super::{error, Router};
    use crate::test_util::*;
    use futures::Future;
    use linkerd2_stack::NewService;
    use std::time::Duration;
    use std::usize;
    use tower::Service;

    impl<N> Router<Request, Recognize, N>
    where
        N: NewService<usize> + Clone,
        N::Service: tower::Service<Request, Response = usize> + Clone,
        <N::Service as tower::Service<Request>>::Error: Into<error::Error>,
    {
        fn call_ok(&mut self, request: impl Into<Request>) -> usize {
            let request = request.into();
            let msg = format!("router.call({:?}) should succeed", request);
            self.call(request).wait().expect(&msg)
        }

        fn call_err(&mut self, request: impl Into<Request>) -> error::Error {
            let request = request.into();
            let msg = format!("router.call({:?}) should error", request);
            self.call(request.into()).wait().expect_err(&msg)
        }
    }

    #[test]
    fn invalid() {
        let (mut router, _cache_bg) = Router::new(Recognize, Recognize, 1, Duration::from_secs(60));

        let rsp = router.call_err(Request::NotRecognized);
        assert!(rsp.is::<error::NotRecognized>());
    }

    #[test]
    fn cache_limited_by_capacity() {
        use futures::future;
        use tokio::runtime::current_thread;

        current_thread::run(future::lazy(|| {
            let (mut router, _cache_bg) =
                Router::new(Recognize, Recognize, 1, Duration::from_secs(1));

            let rsp = router.call_ok(2);
            assert_eq!(rsp, 2);

            let rsp = router.call_err(3);
            assert_eq!(
                rsp.downcast_ref::<error::NoCapacity>()
                    .expect("error should be NoCapacity")
                    .0,
                1
            );

            Ok(())
        }))
    }

    #[test]
    fn services_cached() {
        let (mut router, _cache_bg) = Router::new(Recognize, Recognize, 1, Duration::from_secs(60));

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 2);

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 4);
    }

    #[test]
    fn poll_ready_is_called_first() {
        let (mut router, _cache_bg) = Router::new(
            Recognize,
            MultiplyAndAssign::new(usize::MAX),
            1,
            Duration::from_secs(60),
        );

        let err = router.call_err(2);
        assert_eq!(
            err.downcast_ref::<MulError>().expect("should be MulError"),
            &MulError::AtMax,
        );
    }

    #[test]
    fn load_shed_from_inner_services() {
        use tower_load_shed::error::Overloaded;

        let (mut router, _cache_bg) = Router::new(
            Recognize,
            MultiplyAndAssign::never_ready(),
            1,
            Duration::from_secs(1),
        );

        let err = router.call_err(2);
        assert!(err.downcast_ref::<Overloaded>().is_some(), "Not overloaded",);
    }
}
