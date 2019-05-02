extern crate futures;
extern crate indexmap;
extern crate linkerd2_stack as stack;
extern crate tower_load_shed;
extern crate tower_service as svc;

use futures::{Async, Future, Poll};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower_load_shed::LoadShed;

mod cache;
pub mod error;

use self::cache::Cache;

/// Routes requests based on a configurable `Key`.
pub struct Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    inner: Arc<Inner<Req, Rec, Mk>>,
}

/// Provides a strategy for routing a Request to a Service.
///
/// Implementors must provide a `Key` type that identifies each unique route. The
/// `recognize()` method is used to determine the target for a given request. This target is
/// used to look up a route in a cache (i.e. in `Router`), or can be passed to
/// `bind_service` to instantiate the identified route.
pub trait Recognize<Request> {
    /// Identifies a Route.
    type Target: Eq + Hash;
    /// Determines the target for a route to handle the given request.

    fn recognize(&self, req: &Request) -> Option<Self::Target>;
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

pub struct ResponseFuture<Req, Svc>
where
    Svc: svc::Service<Req>,
    Svc::Error: Into<error::Error>,
{
    state: State<Req, LoadShed<Svc>>,
}

struct Inner<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    recognize: Rec,
    make: Mk,
    cache: Mutex<Cache<Rec::Target, LoadShed<Mk::Value>>>,
}

enum State<Req, Svc>
where
    Svc: svc::Service<Req>,
    Svc::Error: Into<error::Error>,
{
    Init(Option<Req>, Svc),
    Called(Svc::Future),
    Error(Option<error::Error>),
}

// ===== impl Recognize =====

impl<R, T, F> Recognize<R> for F
where
    T: Eq + Hash,
    F: Fn(&R) -> Option<T>,
{
    type Target = T;

    fn recognize(&self, req: &R) -> Option<T> {
        (self)(req)
    }
}

// ===== impl Router =====

impl<Req, Rec, Mk> Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req> + Clone,
{
    pub fn new(recognize: Rec, make: Mk, capacity: usize, max_idle_age: Duration) -> Self {
        Router {
            inner: Arc::new(Inner {
                recognize,
                make,
                cache: Mutex::new(Cache::new(capacity, max_idle_age)),
            }),
        }
    }
}

impl<Req, Rec, Mk, Svc> svc::Service<Req> for Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target, Value = Svc>,
    Svc: svc::Service<Req> + Clone,
    Svc::Error: Into<error::Error>,
{
    type Response = <Mk::Value as svc::Service<Req>>::Response;
    type Error = error::Error;
    type Future = ResponseFuture<Req, Mk::Value>;

    /// Always ready to serve.
    ///
    /// Graceful backpressure is **not** supported at this level, since each request may
    /// be routed to different resources. Instead, requests should be issued and each
    /// route should support a queue of requests.
    ///
    /// TODO Attempt to free capacity in the router.
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

        let cache = &mut *self.inner.cache.lock().expect("lock router cache");

        // First, try to load a cached route for `target`.
        if let Some(service) = cache.access(&target) {
            return ResponseFuture::new(request, service.clone());
        }

        // Since there wasn't a cached route, ensure that there is capacity for a
        // new one.
        let reserve = match cache.reserve() {
            Ok(r) => r,
            Err(cache::CapacityExhausted { capacity }) => {
                return ResponseFuture::no_capacity(capacity);
            }
        };

        // Bind a new route, send the request on the route, and cache the route.
        let service = LoadShed::new(self.inner.make.make(&target));

        reserve.store(target, service.clone());
        ResponseFuture::new(request, service)
    }
}

impl<Req, Rec, Mk> Clone for Router<Req, Rec, Mk>
where
    Rec: Recognize<Req>,
    Mk: Make<Rec::Target>,
    Mk::Value: svc::Service<Req>,
{
    fn clone(&self) -> Self {
        Router {
            inner: self.inner.clone(),
        }
    }
}

// ===== impl ResponseFuture =====

impl<Req, Svc> ResponseFuture<Req, Svc>
where
    Svc: svc::Service<Req>,
    Svc::Error: Into<error::Error>,
{
    fn new(req: Req, svc: LoadShed<Svc>) -> Self {
        ResponseFuture {
            state: State::Init(Some(req), svc),
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

    fn no_capacity(capacity: usize) -> Self {
        Self::error(error::NoCapacity(capacity).into())
    }
}

impl<Req, Svc> Future for ResponseFuture<Req, Svc>
where
    Svc: svc::Service<Req>,
    Svc::Error: Into<error::Error>,
{
    type Item = <LoadShed<Svc> as svc::Service<Req>>::Response;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use svc::Service;

        loop {
            self.state = match self.state {
                State::Init(ref mut req, ref mut svc) => {
                    match svc.poll_ready().map_err(error::Error::from)? {
                        Async::NotReady => {
                            unreachable!("load shedding services must always be ready");
                        }
                        Async::Ready(()) => {
                            let req = req.take().expect("polled after ready");
                            State::Called(svc.call(req))
                        }
                    }
                }
                State::Called(ref mut fut) => return fut.poll().map_err(error::Error::from),
                State::Error(ref mut err) => return Err(err.take().expect("polled after ready")),
            }
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

        fn recognize(&self, req: &Request) -> Option<Self::Target> {
            match *req {
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

        fn call(&mut self, req: Request) -> Self::Future {
            let n = match req {
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
        Mk: Make<usize>,
        Mk::Value: svc::Service<Request, Response = usize> + Clone,
        <Mk::Value as svc::Service<Request>>::Error: Into<error::Error>,
    {
        fn call_ok(&mut self, req: impl Into<Request>) -> usize {
            let req = req.into();
            let msg = format!("router.call({:?}) should succeed", req);
            self.call(req).wait().expect(&msg)
        }

        fn call_err(&mut self, req: impl Into<Request>) -> error::Error {
            let req = req.into();
            let msg = format!("router.call({:?}) should error", req);
            self.call(req.into()).wait().expect_err(&msg)
        }
    }

    #[test]
    fn invalid() {
        let mut router = Router::new(Recognize, Recognize, 1, Duration::from_secs(0));

        let rsp = router.call_err(Request::NotRecognized);
        assert!(rsp.is::<error::NotRecognized>());
    }

    #[test]
    fn cache_limited_by_capacity() {
        let mut router = Router::new(Recognize, Recognize, 1, Duration::from_secs(1));

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 2);

        let rsp = router.call_err(3);
        assert_eq!(
            rsp.downcast_ref::<error::NoCapacity>()
                .expect("error should be NoCapacity")
                .0,
            1
        );
    }

    #[test]
    fn services_cached() {
        let mut router = Router::new(Recognize, Recognize, 1, Duration::from_secs(0));

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 2);

        let rsp = router.call_ok(2);
        assert_eq!(rsp, 4);
    }

    #[test]
    fn poll_ready_is_called_first() {
        let mut router = Router::new(
            Recognize,
            MultiplyAndAssign::new(usize::MAX),
            1,
            Duration::from_secs(0),
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

        let mut router = Router::new(
            Recognize,
            MultiplyAndAssign::never_ready(),
            1,
            Duration::from_secs(0),
        );

        let err = router.call_err(2);
        assert!(err.downcast_ref::<Overloaded>().is_some(), "Not overloaded",);
    }
}
