#[macro_use]
extern crate futures;
extern crate indexmap;
extern crate linkerd2_stack as stack;
extern crate tokio;
extern crate tokio_timer;
extern crate tower_load_shed;
extern crate tower_service as svc;

use futures::{Async, Future, Poll};
use std::hash::Hash;
use std::time::Duration;
use tokio::sync::lock::{Lock, LockGuard};
use tower_load_shed::LoadShed;

mod cache;
pub mod error;

use self::cache::{Cache, PurgeCache};

/// Routes requests based on a configurable `Key`.
pub struct Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    inner: Inner<Req, Rec, Mk>,
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

pub trait Make<Target> {
    type Value;

    fn make(&self, target: &Target) -> Self::Value;
}

impl<F, Target, V> Make<Target> for F
where
    F: Fn(&Target) -> V,
{
    type Value = V;

    fn make(&self, target: &Target) -> Self::Value {
        (*self)(target)
    }
}

pub struct ResponseFuture<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    state: State<Req, Rec, Mk>,
}

struct Inner<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    recognize: Rec,
    make: Mk,
    cache: Lock<Cache<Rec::Target, LoadShed<Mk::Value>>>,
}

struct LockedCache<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    request: Option<Req>,
    target: Option<Rec::Target>,
    make: Option<Mk>,
    locked_cache: Lock<Cache<Rec::Target, LoadShed<Mk::Value>>>,
}

struct UnlockedCache<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    request: Option<Req>,
    target: Option<Rec::Target>,
    make: Option<Mk>,
    unlocked_cache: LockGuard<Cache<Rec::Target, LoadShed<Mk::Value>>>,
}

enum State<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    Init(LockedCache<Req, Rec, Mk>),
    Acquired(UnlockedCache<Req, Rec, Mk>),
    Call(Option<Req>, Option<LoadShed<Mk::Value>>),
    Called(<LoadShed<Mk::Value> as svc::Service<Req>>::Future),
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

// ===== impl Router =====

impl<Req, Rec, Mk> Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    <Mk as Make<Rec::Target>>::Value: Clone,
    Mk::Value: svc::Service<Req>,
{
    pub fn new(
        recognize: Rec,
        make: Mk,
        capacity: usize,
        max_idle_age: Duration,
    ) -> (Self, PurgeCache<Rec::Target, LoadShed<Mk::Value>>) {
        let (cache, cache_bg) = Cache::new(capacity, max_idle_age);

        (
            Router {
                inner: Inner {
                    recognize,
                    make,
                    cache,
                },
            },
            cache_bg,
        )
    }

    pub fn new_lazy(recognize: Rec, make: Mk, capacity: usize, max_idle_age: Duration) -> Self {
        let (cache, _) = Cache::new(capacity, max_idle_age);

        (Router {
            inner: Inner {
                recognize,
                make,
                cache,
            },
        })
    }
}

impl<Req, Rec, Mk> svc::Service<Req> for Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target> + Clone,
    Mk::Value: svc::Service<Req> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    type Response = <Mk::Value as svc::Service<Req>>::Response;
    type Error = error::Error;
    type Future = ResponseFuture<Req, Rec, Mk>;

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

        return ResponseFuture::new(
            request,
            target,
            self.inner.make.clone(),
            self.inner.cache.clone(),
        );
    }
}

impl<Req, Rec, Mk> Clone for Router<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone,
    Mk: Make<Rec::Target> + Clone,
    Mk::Value: svc::Service<Req>,
{
    fn clone(&self) -> Self {
        Router {
            inner: self.inner.clone(),
        }
    }
}

// ===== impl ResponseFuture =====

impl<Req, Rec, Mk> ResponseFuture<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    fn new(
        request: Req,
        target: Rec::Target,
        make: Mk,
        cache: Lock<Cache<Rec::Target, LoadShed<Mk::Value>>>,
    ) -> Self {
        ResponseFuture {
            state: State::Init(LockedCache {
                request: Some(request),
                target: Some(target),
                make: Some(make),
                locked_cache: cache,
            }),
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

impl<Req, Rec, Mk> Future for ResponseFuture<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req> + Clone,
    <Mk::Value as svc::Service<Req>>::Error: Into<error::Error>,
{
    type Item = <LoadShed<Mk::Value> as svc::Service<Req>>::Response;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use svc::Service;

        loop {
            self.state = match self.state {
                State::Init(ref mut state) => {
                    let mut cache = match state.locked_cache.poll_lock() {
                        Async::Ready(lock) => lock,
                        Async::NotReady => return Ok(Async::NotReady),
                    };

                    let request = state.request.take().expect("polled after ready");
                    let target = state.target.take().expect("polled after ready");
                    let make = state.make.take().expect("polled after ready");

                    State::Acquired(UnlockedCache {
                        request: Some(request),
                        target: Some(target),
                        make: Some(make),
                        unlocked_cache: cache,
                    })
                }
                State::Acquired(ref mut state) => {
                    // FIXME(kleimkuhler): This match arm is a bit of a hack.
                    // It is a closure that is immediately executed. The
                    // reason for this is because of the mutable borrow that
                    // happens when we check if the target already exists in
                    // the cache. It should no longer be an issue with NLL.
                    (|| {
                        let request = state.request.take().expect("polled after ready");
                        let target = state.target.take().expect("polled after ready");

                        // If the target exists in the cache, get the service
                        // and call it with `request`.
                        if let Some(service) = state.unlocked_cache.access(&target) {
                            return State::Call(Some(request), Some(service));
                        }

                        // Since there wasn't a cached route, ensure that
                        // there is capacity for a new one
                        let capacity = state.unlocked_cache.capacity();
                        match state.unlocked_cache.reserve() {
                            Ok(Async::Ready(reserve)) => {
                                // Make a new service for the route
                                let make = state.make.take().expect("polled after ready");
                                let service = LoadShed::new(make.make(&target));

                                // Cache the service and send the request on the route
                                reserve.store(target, service.clone());
                                State::Call(Some(request), Some(service))
                            }
                            Ok(Async::NotReady) => {
                                State::Error(Some(error::NoCapacity(capacity).into()))
                            }
                            Err(_) => panic!("Cache::reserve must not fail"),
                        }
                    })()
                }
                State::Call(ref mut request, ref mut service) => {
                    let mut service = service.take().expect("polled after ready");

                    match service.poll_ready() {
                        Ok(Async::Ready(())) => {
                            let request = request.take().expect("polled after ready");
                            State::Called(service.call(request))
                        }
                        Ok(Async::NotReady) => {
                            unreachable!("load shedding services must always be ready");
                        }
                        Err(err) => State::Error(Some(err)),
                    }
                }
                State::Called(ref mut fut) => return fut.poll().map_err(Into::into),
                State::Error(ref mut err) => return Err(err.take().expect("polled after ready")),
            }
        }
    }
}

// ===== impl Inner =====

impl<Req, Rec, Mk> Clone for Inner<Req, Rec, Mk>
where
    Rec: Recognize<Req> + Clone,
    Mk: Make<Rec::Target> + Clone,
    Mk::Value: svc::Service<Req>,
{
    fn clone(&self) -> Self {
        Inner {
            recognize: self.recognize.clone(),
            make: self.make.clone(),
            cache: self.cache.clone(),
        }
    }
}

#[cfg(test)]
mod test_util {
    use super::Make;
    use futures::{future, Async, Poll};
    use std::cell::Cell;
    use std::fmt;
    use std::rc::Rc;
    use svc::Service;

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

    impl Make<usize> for Recognize {
        type Value = MultiplyAndAssign;

        fn make(&self, _: &usize) -> Self::Value {
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

    impl Make<usize> for MultiplyAndAssign {
        type Value = MultiplyAndAssign;

        fn make(&self, _: &usize) -> Self::Value {
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
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{:?}", self)
        }
    }

    impl std::error::Error for MulError {}
}

#[cfg(test)]
mod tests {
    use super::Make;
    use super::{error, Router};
    use futures::Future;
    use std::time::Duration;
    use std::usize;
    use svc::Service;
    use test_util::*;

    impl<Mk> Router<Request, Recognize, Mk>
    where
        Mk: Make<usize> + Clone,
        Mk::Value: svc::Service<Request, Response = usize> + Clone,
        <Mk::Value as svc::Service<Request>>::Error: Into<error::Error>,
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
