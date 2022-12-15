use crate::http::{router::FindRoute, HttpRouteNotFound};
use futures::{future, FutureExt, Stream, StreamExt, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Param, Proxy, Service};
use std::{
    collections::{
        hash_map::{self, HashMap},
        HashSet,
    },
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

/// A router that uses a `Proxy` that's built for each route policy.
pub struct NewProxyRouter<M, N, F> {
    new_proxy: M,
    new_service: N,
    _f: PhantomData<fn(F)>,
}

pub struct ProxyRouter<T, N, P, S, F: FindRoute> {
    new_route: N,
    inner: S,
    target: T,
    rx: Pin<Box<dyn Stream<Item = F> + Send + 'static>>,
    http_routes: F,
    proxies: HashMap<F::Route, P>,
}

// === impl NewProxyRouter ===

impl<M: Clone, N, F> NewProxyRouter<M, N, F> {
    pub fn layer(new_proxy: M) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_service| Self {
            new_service,
            new_proxy: new_proxy.clone(),
            _f: PhantomData,
        })
    }
}

impl<M: Clone, N: Clone, F> Clone for NewProxyRouter<M, N, F> {
    fn clone(&self) -> Self {
        Self {
            new_proxy: self.new_proxy.clone(),
            new_service: self.new_service.clone(),
            _f: PhantomData,
        }
    }
}

impl<T, M, N, F> NewService<T> for NewProxyRouter<M, N, F>
where
    T: Param<Pin<Box<dyn Stream<Item = F> + Send + 'static>>> + Clone,
    F: FindRoute,
    N: NewService<T> + Clone,
    M: NewService<(F::Route, T)> + Clone,
{
    type Service = ProxyRouter<T, M, M::Service, N::Service, F>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut rx = target.param();
        let inner = self.new_service.new_service(target.clone());
        let http_routes = rx
            .next()
            .now_or_never()
            // XXX(eliz): this is gross
            .expect("route stream should always have a first item ready")
            .expect("stream should not be over");
        let mut router = ProxyRouter {
            inner,
            target,
            rx,
            http_routes,
            proxies: HashMap::new(),
            new_route: self.new_proxy.clone(),
        };
        router.update_route_policies();
        router
    }
}

// === impl ProxyRouter ===

impl<T, N, P, S, F> ProxyRouter<T, N, P, S, F>
where
    F: FindRoute,
{
    fn update_route_policies(&mut self)
    where
        T: Clone,
        N: NewService<(F::Route, T), Service = P> + Clone,
    {
        self.http_routes.with_routes(|route_policies| {
            for route in route_policies {
                if let hash_map::Entry::Vacant(ent) = self.proxies.entry(route) {
                    let route = ent.key().clone();
                    let svc = self.new_route.new_service((route, self.target.clone()));
                    ent.insert(svc);
                }
            }
        });
    }
}

impl<B, T, N, P, S, F, Rsp> Service<http::Request<B>> for ProxyRouter<T, N, P, S, F>
where
    T: Clone,
    N: NewService<(F::Route, T), Service = P> + Clone,
    P: Proxy<http::Request<B>, S, Request = http::Request<B>, Response = Rsp>,
    S: Service<http::Request<B>, Response = Rsp>,
    Error: From<S::Error>,
    F: FindRoute,
{
    type Response = Rsp;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<P::Future, fn(P::Error) -> Error>,
        future::Ready<Result<S::Response, Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let http_routes = match self.rx.poll_next_unpin(cx) {
            Poll::Pending => return Poll::Ready(Ok(())),
            Poll::Ready(None) => {
                return Poll::Ready(Err(anyhow::anyhow!("route stream closed").into()))
            }
            Poll::Ready(Some(update)) => update,
        };

        // If the routes have been updated, update the cache.

        // XXX(eliza): this will unify all routes that share the same policy to
        // have a single service...which is nice, unless we want e.g. different
        // metrics for each route. if we want that, we should probably include
        // the metric labels in the policy, i think?
        let route_policies = http_routes.with_routes(|routes| routes.collect::<HashSet<_>>());

        tracing::debug!(routes = %route_policies.len(), "Updating client policy HTTP routes");
        self.http_routes = http_routes;

        // Clear out defunct routes before building any missing routes.
        self.proxies.retain(|r, _| route_policies.contains(r));
        self.update_route_policies();

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        match self.http_routes.find_route(&req) {
            Some(policy) => {
                let future = self
                    .proxies
                    .get(policy)
                    .expect("route must exist")
                    .proxy(&mut self.inner, req)
                    .map_err(Into::into as fn(_) -> _);
                future::Either::Left(future)
            }
            None => future::Either::Right(future::ready(Err(HttpRouteNotFound::default().into()))),
        }
    }
}
