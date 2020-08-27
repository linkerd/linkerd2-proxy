use super::{Receiver, RequestMatch, Route, Routes};
use linkerd2_stack::{layer, NewService, Proxy};
use std::{
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::trace;

pub fn layer<M, N: Clone, R>(
    new_route: N,
) -> impl layer::Layer<M, Service = NewRequestRoute<M, N, R>> {
    let default = Route::default();
    layer::mk(move |inner| NewRequestRoute {
        inner,
        new_route: new_route.clone(),
        default: default.clone(),
        _route: PhantomData,
    })
}

pub struct NewRequestRoute<M, N, R> {
    inner: M,
    new_route: N,
    default: Route,
    _route: PhantomData<R>,
}

pub struct RequestRoute<T, S, N, R> {
    target: T,
    rx: Receiver,
    inner: S,
    new_route: N,
    routes: Vec<(RequestMatch, Route)>,
    proxies: HashMap<Route, R>,
    default: R,
}

impl<T, M, N> tower::Service<(T, Receiver)> for NewRequestRoute<M, N, N::Service>
where
    T: Clone + Send + 'static,
    M: tower::Service<(T, Receiver)>,
    M::Future: Send + 'static,
    N: NewService<(T, Route)> + Clone + Send + 'static,
{
    type Response = RequestRoute<T, M::Response, N, N::Service>;
    type Error = M::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, M::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, (target, rx): (T, Receiver)) -> Self::Future {
        let fut = self.inner.call((target.clone(), rx.clone()));
        let new_route = self.new_route.clone();
        let default_route = self.default.clone();
        Box::pin(async move {
            let inner = fut.await?;
            let default = new_route.new_service((target.clone(), default_route));
            Ok(RequestRoute {
                rx,
                target,
                inner,
                default,
                new_route,
                routes: Vec::new(),
                proxies: HashMap::new(),
            })
        })
    }
}

impl<B, T, N, S, R> tower::Service<http::Request<B>> for RequestRoute<T, S, N, R>
where
    B: Send + 'static,
    T: Clone,
    N: NewService<(T, Route), Service = R> + Clone,
    R: Proxy<http::Request<B>, S>,
    S: tower::Service<R::Request>,
{
    type Response = R::Response;
    type Error = R::Error;
    type Future = R::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut update = None;
        while let Poll::Ready(Some(up)) = self.rx.poll_recv_ref(cx) {
            update = Some(up.clone());
        }
        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        if let Some(Routes { routes, .. }) = update {
            let mut proxies = HashMap::with_capacity(routes.len());
            for (_, ref route) in &routes {
                // Reuse the prior services whenever possible.
                let proxy = self.proxies.remove(&route).unwrap_or_else(|| {
                    self.new_route
                        .new_service((self.target.clone(), route.clone()))
                });
                proxies.insert(route.clone(), proxy);
            }
            self.routes = routes;
            self.proxies = proxies;
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!(?condition, "Using configured route");
                return self.proxies[route].proxy(&mut self.inner, req);
            }
        }

        trace!("Using default route");
        self.default.proxy(&mut self.inner, req)
    }
}
