use super::{RequestMatch, Route};
use crate::{Profile, Receiver, ReceiverStream};
use futures::{future, prelude::*};
use linkerd_error::{Error, Result};
use linkerd_stack::{NewService, Param, Proxy, RecognizeRoute, Service, ServiceExt};
use std::{
    collections::{HashMap, HashSet},
    task::{Context, Poll},
};
use tracing::{debug, trace};

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

#[derive(Copy, Clone, Debug, Default)]
pub struct NewRecognize(());

#[derive(Clone, Debug)]
pub struct Recognize<T> {
    rx: Receiver,
    target: T,
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
        for (condition, route) in &self.rx.borrow().http_routes {
            if condition.is_match(req) {
                trace!(?condition, "Using configured route");
                return Ok((Some(route.clone()), self.target.clone()));
            }
        }

        trace!("No matching routes");
        Ok((None, self.target.clone()))
    }
}

// === impl NewProxyRouter ===

impl<T, M, N> NewService<T> for NewProxyRouter<M, N>
where
    T: Param<Receiver> + Clone,
    N: NewService<T> + Clone,
    M: NewService<(Option<Route>, T)> + Clone,
{
    type Service = ProxyRouter<T, M, M::Service, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let inner = self.inner.new_service(target.clone());
        ProxyRouter {
            inner,
            target,
            rx: rx.into(),
            http_routes: Vec::new(),
            new_proxy: self.new_proxy.clone(),
        }
    }
}

// === impl ProxyRouter ===

impl<B, T, N, P, S, Rsp> Service<http::Request<B>> for ProxyRouter<T, N, P, S>
where
    T: Clone,
    N: NewService<(Route, T), Service = P> + Clone,
    P: Proxy<http::Request<B>, S, Response = Rsp>,
    S: Service<P::Request, Response = Rsp>,
    S::Error: Into<Error>,
{
    type Response = Rsp;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<S::Future, fn(S::Error) -> Error>,
        future::MapErr<P::Future, fn(P::Error) -> Error>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if let Poll::Ready(Some(Profile { http_routes, .. })) = self.rx.poll_next_unpin(cx) {
            debug!(routes = %http_routes.len(), "Updating HTTP routes");
            let routes = http_routes
                .into_iter()
                .map(|(_, r)| r)
                .collect::<HashSet<_>>();
            self.proxies.retain(|r, _| routes.contains(&r));
            for route in routes.into_iter() {
                self.proxies
                    .entry(route.clone())
                    .or_insert_with(|| self.new_proxy.new_service((route, self.target.clone())));
            }
        }

        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let (route, target) = match self.recognize.recognize(&req) {
            Ok(r) => r,
            Err(e) => {
                debug!(?e, "No matching route");
                return future::Either::Left(self.inner.call(req));
            }
        };
        for (ref condition, ref route) in &self.http_routes {
            if condition.is_match(&req) {
                trace!(?condition, "Using configured route");
                let svc = self
                    .inner
                    .new_service((Some(route.clone()), self.target.clone()));

                return future::Either::Right(svc.oneshot(req));
            }
        }

        trace!("No routes matched");
        future::Either::Left(self.default.call(req))
    }
}
