use crate::metrics::{handle_time, Scoped, Stats};
use futures::{future, try_ready, Future, Poll};
use http::{Request, Response};
use linkerd2_proxy_transport::tls;
use std::marker::PhantomData;
use tower::retry as tower_retry;
pub use tower::retry::budget::Budget;
use tracing::trace;

pub trait CanRetry {
    type Retry: Retry + Clone;
    fn can_retry(&self) -> Option<Self::Retry>;
}

pub trait Retry: Sized {
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

pub struct Layer<S, K, A, B> {
    registry: S,
    _p: PhantomData<(K, fn(A) -> B)>,
}

pub struct Stack<M, S, K, A, B> {
    inner: M,
    registry: S,
    _p: PhantomData<(K, fn(A) -> B)>,
}

pub struct MakeFuture<F, R, S> {
    inner: F,
    policy: Option<Policy<R, S>>,
}

pub type Service<R, Svc, St> = tower_retry::Retry<Policy<R, St>, Svc>;

#[derive(Clone)]
pub struct Policy<R, S>(R, S);

// === impl Layer ===

pub fn layer<S, K, A, B>(registry: S) -> Layer<S, K, A, B> {
    Layer {
        registry,
        _p: PhantomData,
    }
}

impl<S: Clone, K, A, B> Clone for Layer<S, K, A, B> {
    fn clone(&self) -> Self {
        Layer {
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<M, S, K, A, B> tower::layer::Layer<M> for Layer<S, K, A, B>
where
    S: Scoped<K> + Clone,
    S::Scope: Clone,
    A: TryClone,
{
    type Service = Stack<M, S, K, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M: Clone, S: Clone, K, A, B> Clone for Stack<M, S, K, A, B> {
    fn clone(&self) -> Self {
        Stack {
            inner: self.inner.clone(),
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

/// impl MakeService
impl<T, M, S, K, A, B> tower::Service<T> for Stack<M, S, K, A, B>
where
    T: CanRetry + Clone,
    M: tower::MakeService<T, Request<A>, Response = Response<B>>,
    M::Service: Clone,
    S: Scoped<K>,
    S::Scope: Clone,
    K: From<T>,
    A: TryClone,
{
    type Response = tower::util::Either<Service<T::Retry, M::Service, S::Scope>, M::Service>;
    type Error = M::MakeError;
    type Future = MakeFuture<M::Future, T::Retry, S::Scope>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let policy = if let Some(retries) = target.can_retry() {
            trace!("stack is retryable");
            let stats = self.registry.scoped(target.clone().into());
            Some(Policy(retries, stats))
        } else {
            None
        };

        let inner = self.inner.make_service(target);
        MakeFuture { inner, policy }
    }
}

// === impl MakeFuture ===

impl<F, R, S> Future for MakeFuture<F, R, S>
where
    F: Future,
{
    type Item = tower::util::Either<Service<R, F::Item, S>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        if let Some(policy) = self.policy.take() {
            Ok(tower::util::Either::A(tower_retry::Retry::new(policy, inner)).into())
        } else {
            Ok(tower::util::Either::B(inner).into())
        }
    }
}

// === impl Policy ===

impl<R, S, A, B, E> tower_retry::Policy<Request<A>, Response<B>, E> for Policy<R, S>
where
    R: Retry + Clone,
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

// TODO this needs to be moved up into the application!
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
