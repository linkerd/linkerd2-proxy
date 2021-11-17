use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future, prelude::*};
use linkerd_error::{Error, Result};
use linkerd_stack::{layer, NewService, Param, Proxy, RecognizeRoute, Service};
use std::{
    collections::{hash_map, HashMap, HashSet},
    task::{Context, Poll},
};
use tracing::{debug, trace};

type NewRouter<N> = linkerd_stack::NewRouter<NewRecognize, N>;

pub fn layer<N>() -> impl layer::Layer<N, Service = NewRouter<N>> + Clone {
    linkerd_stack::NewRouter::layer(NewRecognize(()))
}

#[derive(Clone, Debug)]
pub struct NewProxyRouter<M, N> {
    new_proxy: M,
    new_service: N,
}

#[derive(Debug)]
pub struct ProxyRouter<T, N, P, S> {
    new_proxy: N,
    inner: S,
    target: T,
    rx: ReceiverStream,
    http_routes: Vec<(RequestMatch, Route)>,
    proxies: HashMap<Route, P>,
}

#[derive(Copy, Clone, Debug)]
pub struct NewRecognize(());

#[derive(Clone, Debug)]
pub struct Recognize<T> {
    rx: Receiver,
    target: T,
}

fn route_for_request<'r, B>(
    http_routes: &'r [(RequestMatch, Route)],
    request: &http::Request<B>,
) -> Option<&'r Route> {
    for (request_match, route) in http_routes {
        if request_match.is_match(request) {
            return Some(route);
        }
    }
    None
}

// === impl NewRecognize ===

impl<T: Param<Receiver>> NewService<T> for NewRecognize {
    type Service = Recognize<T>;

    fn new_service(&self, target: T) -> Self::Service {
        Recognize {
            rx: target.param(),
            target,
        }
    }
}

// === impl Recognize ===

impl<T: Clone, B> RecognizeRoute<http::Request<B>> for Recognize<T> {
    type Key = (Option<Route>, T);

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key> {
        let profile = self.rx.borrow();
        let route = route_for_request(&profile.http_routes, req);
        trace!(?route);
        Ok((route.cloned(), self.target.clone()))
    }
}

// === impl NewProxyRouter ===

impl<M: Clone, N> NewProxyRouter<M, N> {
    pub fn layer(new_proxy: M) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |new_service| Self {
            new_service,
            new_proxy: new_proxy.clone(),
        })
    }
}

impl<T, M, N> NewService<T> for NewProxyRouter<M, N>
where
    T: Param<Receiver> + Clone,
    N: NewService<T> + Clone,
    M: NewService<(Route, T)> + Clone,
{
    type Service = ProxyRouter<T, M, M::Service, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let inner = self.new_service.new_service(target.clone());
        ProxyRouter {
            inner,
            target,
            rx: rx.into(),
            http_routes: Vec::new(),
            proxies: HashMap::new(),
            new_proxy: self.new_proxy.clone(),
        }
    }
}

// === impl ProxyRouter ===

type ProxyResponseFuture<F1, E1, F2, E2> = future::Either<
    future::MapErr<F1, fn(E1) -> Error>,
    future::MapErr<F2, fn(E2) -> Error>,
>;

impl<B, T, N, P, S, Rsp> Service<http::Request<B>> for ProxyRouter<T, N, P, S>
where
    T: Clone,
    N: NewService<(Route, T), Service = P> + Clone,
    P: Proxy<http::Request<B>, S, Request = http::Request<B>, Response = Rsp>,
    S: Service<http::Request<B>, Response = Rsp>,
    S::Error: Into<Error>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = ProxyResponseFuture<S::Future, S::Error, P::Future, P::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Poll the inner service first so we don't bother updating routes unless we can actually
        // use them.
        futures::ready!(self.inner.poll_ready(cx).map_err(Into::into))?;

        // If the routes have been updated, update the cache.
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            let routes = http_routes
                .iter()
                .map(|(_, r)| r.clone())
                .collect::<HashSet<_>>();
            self.http_routes = http_routes;

            // Clear out defunct routes before building any missing routes.
            self.proxies.retain(|r, _| routes.contains(r));
            for route in routes.into_iter() {
                if let hash_map::Entry::Vacant(ent) = self.proxies.entry(route) {
                    let proxy = self
                        .new_proxy
                        .new_service((ent.key().clone(), self.target.clone()));
                    ent.insert(proxy);
                }
            }
        }

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        // If the request matches a route, use the route's proxy to wrap the inner service.
        if let Some(route) = route_for_request(&self.http_routes, &req) {
            trace!(?route, "Using route proxy");
            return future::Either::Right(
                self.proxies
                    .get(route)
                    .expect("proxy must exist")
                    .proxy(&mut self.inner, req)
                    .map_err(Into::into),
            );
        }

        // Otherwise, use the inner service directly.
        trace!("No routes matched");
        future::Either::Left(self.inner.call(req).map_err(Into::into))
    }
}
