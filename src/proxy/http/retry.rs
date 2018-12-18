use std::marker::PhantomData;

use futures::future;
use http::{Request, Response};
use tower_retry;

use proxy::http::metrics::{Scoped, Stats};
use svc;

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

impl<T, M, S, K, A, B> svc::Layer<T, T, M> for Layer<S, K, A, B>
where
    T: CanRetry + Clone,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request<A>, Response = Response<B>> + Clone,
    S: Scoped<K> + Clone,
    S::Scope: Clone,
    K: From<T>,
    A: TryClone,
{
    type Value = <Stack<M, S, K, A, B> as svc::Stack<T>>::Value;
    type Error = <Stack<M, S, K, A, B> as svc::Stack<T>>::Error;
    type Stack = Stack<M, S, K, A, B>;

    fn bind(&self, inner: M) -> Self::Stack {
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

impl<T, M, S, K, A, B> svc::Stack<T> for Stack<M, S, K, A, B>
where
    T: CanRetry + Clone,
    M: svc::Stack<T>,
    M::Value: svc::Service<Request<A>, Response = Response<B>> + Clone,
    S: Scoped<K>,
    S::Scope: Clone,
    K: From<T>,
    A: TryClone,
{
    type Value = svc::Either<Service<T::Retry, M::Value, S::Scope>, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(target)?;
        if let Some(retries) = target.can_retry() {
            trace!("stack is retryable");
            let stats = self.registry.scoped(target.clone().into());
            Ok(svc::Either::A(tower_retry::Retry::new(Policy(retries, stats), inner)))
        } else {
            Ok(svc::Either::B(inner))
        }
    }
}

// === impl Policy ===

impl<R, S, A, B, E> ::tower_retry::Policy<Request<A>, Response<B>, E> for Policy<R, S>
where
    R: Retry + Clone,
    S: Stats + Clone,
    A: TryClone,
{
    type Future = future::FutureResult<Self, ()>;

    fn retry(&self, req: &Request<A>, result: Result<&Response<B>, &E>) -> Option<Self::Future> {
        match result {
            Ok(res) => {
                match self.0.retry(req, res) {
                    Ok(()) => {
                        trace!("retrying request");
                        Some(future::ok(self.clone()))
                    },
                    Err(NoRetry::Budget) => {
                        self.1.incr_retry_skipped_budget();
                        None
                    },
                    Err(NoRetry::Success) => None,
                }
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

impl<B: TryClone> TryClone for Request<B> {
    fn try_clone(&self) -> Option<Self> {
        if let Some(body) = self.body().try_clone() {
            let mut clone = Request::new(body);
            *clone.method_mut() = self.method().clone();
            *clone.uri_mut() = self.uri().clone();
            *clone.headers_mut() = self.headers().clone();
            *clone.version_mut() = self.version();

            if let Some(ext) = self.extensions().get::<::proxy::server::Source>() {
                clone.extensions_mut().insert(ext.clone());
            }

            Some(clone)
        } else {
            None
        }
    }
}
