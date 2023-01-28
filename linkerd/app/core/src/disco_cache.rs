use futures::prelude::*;
use linkerd_error::Error;
use linkerd_idle_cache::{Cached, NewIdleCached};
use linkerd_stack::{layer, NewService, Param, Service, ServiceExt};
use std::{
    fmt,
    future::Future,
    hash::Hash,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

#[derive(Clone)]
pub struct NewDiscoveryCache<K, D, N>
where
    K: Eq + Hash,
    D: NewService<K>,
    D::Service: Clone,
{
    inner: N,
    cache: NewIdleCached<K, D>,
}

pub struct CachedDiscovery<Rsp, D, N, S> {
    // This must be held to keep the cache entry alive.
    disco: Cached<D>,

    state: State<Rsp, N, S>,
}

enum State<Rsp, N, S> {
    Init(Option<N>),
    Pending {
        future: Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>,
        inner: N,
    },
    Service(S),
}

impl<K, D, N> NewDiscoveryCache<K, D, N>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: NewService<K> + Clone + 'static,
    D::Service: Clone + Send + Sync + 'static,
{
    pub fn new(inner: N, disco: D, idle: time::Duration) -> Self {
        Self {
            inner,
            cache: NewIdleCached::new(disco, idle),
        }
    }

    pub fn layer(disco: D, idle: time::Duration) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, disco.clone(), idle))
    }
}

impl<T, K, D, DSvc, M, N> NewService<T> for NewDiscoveryCache<K, D, M>
where
    T: Param<K> + Clone,
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    D: NewService<K, Service = DSvc> + 'static,
    DSvc: Service<()> + Clone + Send + Sync + 'static,
    M: NewService<T, Service = N> + Clone,
    N: NewService<DSvc::Response> + Clone,
{
    type Service = CachedDiscovery<DSvc::Response, DSvc, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = target.param();
        let disco = self.cache.new_service(key);
        let inner = self.inner.new_service(target);
        CachedDiscovery {
            disco,
            state: State::Init(Some(inner)),
        }
    }
}

impl<Req, D, N, S> Service<Req> for CachedDiscovery<D::Response, D, N, S>
where
    D: Service<(), Error = Error> + Clone + Send + Sync + 'static,
    D::Response: Clone,
    D::Future: Send + 'static,
    N: NewService<D::Response, Service = S>,
    S: Service<Req, Error = Error>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                // We have an inner service. Wait for it to be ready.
                State::Service(ref mut svc) => return svc.poll_ready(cx),

                // We don't have an inner service, so start discovery so that we
                // can build one.
                State::Init(ref mut inner) => {
                    let disco = self.disco.clone();
                    State::Pending {
                        future: Box::pin(disco.oneshot(())),
                        inner: inner.take().expect("illegal state"),
                    }
                }

                // Waiting for discovery to complete for `target`.
                State::Pending {
                    ref mut future,
                    ref inner,
                } => match futures::ready!(future.poll_unpin(cx)) {
                    Ok(rsp) => {
                        let svc = inner.new_service(rsp);
                        State::Service(svc)
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                },
            };
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if let State::Service(ref mut svc) = self.state {
            return svc.call(req);
        }

        panic!("poll_ready must be called first");
    }
}
