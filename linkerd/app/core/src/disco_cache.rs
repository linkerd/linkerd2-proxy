#![allow(warnings)] // FIXME

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
pub struct NewDiscoveryCache<K, DNew, SNew>
where
    K: Eq + Hash,
    DNew: NewService<K>,
    DNew::Service: Clone,
{
    inner: SNew,
    cache: NewIdleCached<K, DNew>,
}

pub struct CachedDiscovery<T, Rsp, DSvc, SNew, SSvc> {
    target: T,
    new: SNew,
    disco: Cached<DSvc>,
    state: State<Rsp, SSvc>,
}

enum State<Rsp, S> {
    Init,
    Pending(Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>),
    Service(S),
}

impl<K, DNew, SNew> NewDiscoveryCache<K, DNew, SNew>
where
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    DNew: NewService<K> + Clone + 'static,
    DNew::Service: Clone + Send + Sync + 'static,
{
    pub fn new(inner: SNew, disco: DNew, idle: time::Duration) -> Self {
        Self {
            inner,
            cache: NewIdleCached::new(disco, idle),
        }
    }

    pub fn layer(
        disco: DNew,
        idle: time::Duration,
    ) -> impl layer::Layer<SNew, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, disco.clone(), idle))
    }
}

impl<T, K, DNew, DSvc, SNew> NewService<T> for NewDiscoveryCache<K, DNew, SNew>
where
    T: Param<K>,
    K: Clone + fmt::Debug + Eq + Hash + Send + Sync + 'static,
    DNew: NewService<K, Service = DSvc> + 'static,
    DSvc: Service<()> + Clone + Send + Sync + 'static,
    SNew: NewService<DSvc::Response> + Clone,
{
    type Service = CachedDiscovery<T, DSvc::Response, DSvc, SNew, SNew::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let key = target.param();
        let disco = self.cache.new_service(key);
        CachedDiscovery {
            target,
            disco,
            new: self.inner.clone(),
            state: State::Init,
        }
    }
}

impl<Req, T, DSvc, SNew, SSvc> Service<Req> for CachedDiscovery<T, DSvc::Response, DSvc, SNew, SSvc>
where
    DSvc: Service<(), Error = Error> + Clone + Send + Sync + 'static,
    DSvc::Response: Clone,
    DSvc::Future: Send + 'static,
    SNew: NewService<DSvc::Response, Service = SSvc>,
    SSvc: Service<Req, Error = Error> + Clone + Send + Sync + 'static,
{
    type Response = SSvc::Response;
    type Error = SSvc::Error;
    type Future = SSvc::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                // We have a response! We're now ready to clone this service
                State::Service(ref mut svc) => return svc.poll_ready(cx),

                // Discovery for `target` has not yet been started.
                State::Init => {
                    let disco = self.disco.clone();
                    State::Pending(Box::pin(disco.oneshot(())))
                }

                // Waiting for discovery to complete for `target`.
                State::Pending(ref mut f) => match futures::ready!(f.poll_unpin(cx)) {
                    Ok(rsp) => {
                        let svc = self.new.new_service(rsp);
                        State::Service(svc)
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                },
            };
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.state {
            State::Service(ref mut svc) => svc.call(req),
            _ => panic!("poll_ready must be called first"),
        }
    }
}
