use crate::metrics::{handle_time, Scoped, Stats};
use futures::{future, Future, Poll};
use http::{Request, Response};
use linkerd2_error::Error;
use linkerd2_proxy_transport::tls;
use linkerd2_stack::{proxy, NewService, Proxy};
use tower::retry;
pub use tower::retry::budget::Budget;
pub use tower::util::{Oneshot, ServiceExt};
use tracing::trace;

pub trait HasPolicy {
    type Policy: Policy + Clone;

    fn retry_policy(&self) -> Option<Self::Policy>;
}

pub trait Policy: Sized {
    fn retry<B1, B2>(&self, req: &Request<B1>, res: &Response<B2>) -> Result<(), NoRetry>;

    fn clone_request<B: TryClone>(&self, req: &Request<B>) -> Option<Request<B>>;
}

pub enum NoRetry {
    Success,
    Budget,
}

pub trait TryClone: Sized {
    fn try_clone(&self) -> Option<Self>;
}

#[derive(Clone, Debug)]
pub struct Layer<R> {
    registry: R,
}

#[derive(Clone, Debug)]
pub struct NewRetry<M, R> {
    registry: R,
    inner: M,
}

#[derive(Clone, Debug)]
pub enum Retry<R, S> {
    Disabled(S),
    Enabled(R, S),
}

pub enum ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    Disabled(P::Future),
    Retry(Oneshot<retry::Retry<R, proxy::Service<P, S>>, Req>),
}

#[derive(Clone)]
pub struct PolicyStats<R, S>(R, S);

// === impl Layer ===

pub fn layer<R>(registry: R) -> Layer<R> {
    Layer { registry }
}

impl<M, R: Clone> tower::layer::Layer<M> for Layer<R> {
    type Service = NewRetry<M, R>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === impl Stack ===

impl<T, M, R> NewService<T> for NewRetry<M, R>
where
    T: HasPolicy + Clone,
    M: NewService<T>,
    M::Service: Clone,
    R: Scoped<T>,
    R::Scope: Clone,
{
    type Service = Retry<PolicyStats<T::Policy, R::Scope>, M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        if let Some(policy) = target.retry_policy() {
            trace!("retryable");
            let policy = PolicyStats(policy, self.registry.scoped(target.clone()));
            Retry::Enabled(policy, self.inner.new_service(target))
        } else {
            Retry::Disabled(self.inner.new_service(target))
        }
    }
}

// === impl Policy ===

impl<R, S, A, B, E> retry::Policy<Request<A>, Response<B>, E> for PolicyStats<R, S>
where
    R: Policy + Clone,
    S: Stats + Clone,
    A: TryClone,
{
    type Future = future::FutureResult<Self, ()>;

    fn retry(&self, req: &Request<A>, result: Result<&Response<B>, &E>) -> Option<Self::Future> {
        match result {
            Ok(res) => match self.0.retry(req, res) {
                Ok(()) => {
                    trace!("retrying request");
                    Some(future::ok(self.clone()))
                }
                Err(NoRetry::Budget) => {
                    self.1.incr_retry_skipped_budget();
                    None
                }
                Err(NoRetry::Success) => None,
            },
            Err(_err) => {
                trace!("cannot retry transport error");
                None
            }
        }
    }

    fn clone_request(&self, req: &Request<A>) -> Option<Request<A>> {
        if let Some(clone) = self.0.clone_request(req) {
            trace!("cloning request");
            Some(clone)
        } else {
            trace!("request could not be cloned");
            None
        }
    }
}

// === impl Retry ===

impl<R, Req, S, P> Proxy<Req, S> for Retry<R, P>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = Error;
    type Future = ResponseFuture<R, P, S, Req>;

    fn proxy(&self, svc: &mut S, req: Req) -> Self::Future {
        match self {
            Retry::Disabled(ref inner) => ResponseFuture::Disabled(inner.proxy(svc, req)),
            Retry::Enabled(ref policy, ref inner) => {
                let svc = inner.clone().into_service(svc.clone());
                let retry = retry::Retry::new(policy.clone(), svc);
                ResponseFuture::Retry(retry.oneshot(req))
            }
        }
    }
}

impl<R, P, S, Req> Future for ResponseFuture<R, P, S, Req>
where
    R: retry::Policy<Req, P::Response, Error> + Clone,
    P: Proxy<Req, S> + Clone,
    S: tower::Service<P::Request> + Clone,
    S::Error: Into<Error>,
{
    type Item = P::Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self {
            ResponseFuture::Disabled(ref mut f) => f.poll().map_err(Into::into),
            ResponseFuture::Retry(ref mut f) => f.poll().map_err(Into::into),
        }
    }
}

impl<B: TryClone> TryClone for Request<B> {
    fn try_clone(&self) -> Option<Self> {
        if let Some(body) = self.body().try_clone() {
            let mut clone = Request::new(body);
            *clone.method_mut() = self.method().clone();
            *clone.uri_mut() = self.uri().clone();
            *clone.headers_mut() = self.headers().clone();
            *clone.version_mut() = self.version();

            if let Some(ext) = self.extensions().get::<tls::accept::Meta>() {
                clone.extensions_mut().insert(ext.clone());
            }

            // Count retries toward the request's total handle time.
            if let Some(ext) = self.extensions().get::<handle_time::Tracker>() {
                clone.extensions_mut().insert(ext.clone());
            }

            Some(clone)
        } else {
            None
        }
    }
}
