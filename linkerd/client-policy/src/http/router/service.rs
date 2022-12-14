use crate::http::{router::FindRoute, HttpRouteNotFound};
use futures::{future, FutureExt, Stream, StreamExt, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{
        hash_map::{self, HashMap},
        HashSet,
    },
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

/// A router that uses a `Service` that's built for each route policy.
pub struct NewServiceRouter<N, F>(N, PhantomData<fn(F)>);

pub struct ServiceRouter<T, N, S, F: FindRoute> {
    new_route: N,
    target: T,
    rx: Pin<Box<dyn Stream<Item = F> + Send + 'static>>,
    http_routes: F,
    services: HashMap<F::Route, S>,
}

// === impl NewServiceRouter ===

impl<N, F> NewServiceRouter<N, F> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self::new)
    }

    pub fn new(new_route: N) -> Self {
        Self(new_route, PhantomData)
    }
}

impl<T, N, F> NewService<T> for NewServiceRouter<N, F>
where
    T: Param<Pin<Box<dyn Stream<Item = F> + Send + 'static>>> + Clone,
    F: FindRoute,
    N: NewService<(F::Route, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service, F>;

    fn new_service(&self, target: T) -> Self::Service {
        let mut rx = target.param();
        let http_routes = rx
            .next()
            .now_or_never()
            // XXX(eliz): this is gross
            .expect("route stream should always have a first item ready")
            .expect("stream should not be over");
        let mut router = ServiceRouter {
            target,
            rx,
            http_routes,
            services: HashMap::new(),
            new_route: self.0.clone(),
        };
        router.update_route_policies();
        router
    }
}

// === impl ServiceRouter ===

impl<T, N, S, F> ServiceRouter<T, N, S, F>
where
    F: FindRoute,
{
    fn update_route_policies(&mut self)
    where
        T: Clone,
        N: NewService<(F::Route, T), Service = S> + Clone,
    {
        self.http_routes.with_routes(|route_policies| {
            for route in route_policies {
                if let hash_map::Entry::Vacant(ent) = self.services.entry(route) {
                    let route = ent.key().clone();
                    let svc = self.new_route.new_service((route, self.target.clone()));
                    ent.insert(svc);
                }
            }
        });
    }
}

impl<B, T, N, S, F> Service<http::Request<B>> for ServiceRouter<T, N, S, F>
where
    T: Clone,
    N: NewService<(F::Route, T), Service = S> + Clone,
    S: Service<http::Request<B>> + Clone,
    Error: From<S::Error>,
    F: FindRoute,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::MapErr<Oneshot<S, http::Request<B>>, fn(S::Error) -> Error>,
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
        self.services.retain(|r, _| route_policies.contains(r));
        self.update_route_policies();

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        match self.http_routes.find_route(&req) {
            Some(policy) => {
                let future = self
                    .services
                    .get(policy)
                    .expect("route must exist")
                    .clone()
                    .oneshot(req)
                    .map_err(Into::into as fn(_) -> _);
                future::Either::Left(future)
            }
            None => future::Either::Right(future::ready(Err(HttpRouteNotFound::default().into()))),
        }
    }
}
