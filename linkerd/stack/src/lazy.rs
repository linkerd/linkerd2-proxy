use crate::{NewService, Service};
use parking_lot::Mutex;
use std::{
    sync::Arc,
    task::{Context, Poll},
};

/// Lazily constructs an inner stack when the service is first used.
#[derive(Clone, Debug)]
pub struct NewLazy<N>(N);

#[derive(Debug)]
pub struct Lazy<T, N, S> {
    inner: Option<S>,
    shared: Arc<Mutex<Shared<T, N, S>>>,
}

#[derive(Debug)]
enum Shared<T, N, S> {
    Uninit { inner: N, target: T },
    Service(S),
    Invalid,
}

/// === impl NewLazy ===

impl<N> NewLazy<N> {
    pub fn layer() -> impl super::layer::Layer<N, Service = Self> + Clone {
        super::layer::mk(Self)
    }
}

impl<T, N> NewService<T> for NewLazy<N>
where
    N: NewService<T> + Clone,
{
    type Service = Lazy<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        Lazy {
            inner: None,
            shared: Arc::new(Mutex::new(Shared::Uninit {
                inner: self.0.clone(),
                target,
            })),
        }
    }
}

/// === impl Lazy ===

impl<Req, T, N, S> Service<Req> for Lazy<T, N, S>
where
    N: NewService<T, Service = S>,
    S: Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            if let Some(inner) = self.inner.as_mut() {
                return inner.poll_ready(cx);
            }

            let mut shared = self.shared.lock();
            *shared = match std::mem::take(&mut *shared) {
                Shared::Uninit { inner, target } => {
                    let svc = inner.new_service(target);
                    self.inner = Some(svc.clone());
                    Shared::Service(svc)
                }

                Shared::Service(svc) => {
                    self.inner = Some(svc.clone());
                    Shared::Service(svc)
                }

                Shared::Invalid => unreachable!(),
            };

            debug_assert!(self.inner.is_some());
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.as_mut().expect("polled").call(req)
    }
}

impl<T, N, S: Clone> Clone for Lazy<T, N, S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// === impl Shared ===

impl<T, N, S> Default for Shared<T, N, S> {
    fn default() -> Self {
        Shared::Invalid
    }
}
