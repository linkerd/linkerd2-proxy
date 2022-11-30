use super::{Route, RoutePolicy};
use crate::Receiver;
use futures::{future::MapErr, FutureExt, TryFutureExt};
use linkerd_error::Error;
use linkerd_stack::{layer, NewService, Oneshot, Param, Service, ServiceExt};
use std::{
    collections::{
        hash_map::{self, HashMap},
        HashSet,
    },
    sync::Arc,
    task::{Context, Poll},
};
use tokio_util::sync::ReusableBoxFuture;

/// A router that uses a `Service` that's built for each route policy (with a
/// fallback service when no route is matched).
///
/// This router behaves similarly to the
/// `linkerd_service_profiles::http::ServiceRouter`, but it operates on
/// HTTPRoute policies rather than ServiceProfile policies.
#[derive(Clone, Debug)]
pub struct NewServiceRouter<N>(N);

#[derive(Debug)]
pub struct ServiceRouter<T, N, S> {
    new_route: N,
    target: T,
    changed: ReusableBoxFuture<'static, Result<Receiver, Error>>,
    http_routes: Arc<[Route]>,
    services: HashMap<RoutePolicy, S>,
    default: S,
}

// === impl NewServiceRouter ===

impl<N> NewServiceRouter<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(Self)
    }
}

impl<T, N> NewService<T> for NewServiceRouter<N>
where
    T: Param<Receiver> + Clone,
    N: NewService<(Option<Route>, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target.param();
        let default = self.0.new_service((None, target.clone()));
        ServiceRouter {
            default,
            target,
            changed: ReusableBoxFuture::new(Box::pin(changed(rx))),
            http_routes: Arc::new([]),
            services: HashMap::new(),
            new_route: self.0.clone(),
        }
    }
}

// === impl ServiceRouter ===

impl<B, T, N, S> Service<http::Request<B>> for ServiceRouter<T, N, S>
where
    T: Clone,
    N: NewService<(Option<RoutePolicy>, T), Service = S> + Clone,
    S: Service<http::Request<B>> + Clone,
    Error: From<S::Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = MapErr<Oneshot<S, http::Request<B>>, fn(S::Error) -> Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut policy = match self.changed.poll_unpin(cx) {
            Poll::Pending => return Poll::Ready(Ok(())),
            Poll::Ready(update) => update?,
        };

        let http_routes = policy.borrow_and_update().http_routes.clone();

        // If the routes have been updated, update the cache.
        tracing::debug!(routes = %http_routes.len(), "Updating client policy HTTP routes");
        // XXX(eliza): this will unify all routes that share the same policy to
        // have a single service...which is nice, unless we want e.g. different
        // metrics for each route. if we want that, we should probably include
        // the metric labels in the policy, i think?
        let route_policies = http_routes
            .iter()
            .flat_map(|route| route.rules.iter())
            .map(|rule| rule.policy.clone())
            .collect::<HashSet<_>>();
        self.http_routes = http_routes;

        // Clear out defunct routes before building any missing routes.
        self.services.retain(|r, _| route_policies.contains(r));
        for route in route_policies.into_iter() {
            if let hash_map::Entry::Vacant(ent) = self.services.entry(route) {
                let route = ent.key().clone();
                let svc = self
                    .new_route
                    .new_service((Some(route), self.target.clone()));
                ent.insert(svc);
            }
        }

        // wait for the next change
        self.changed.set(changed(policy));

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = match super::find(&self.http_routes, &req) {
            Some((req_match, policy)) => {
                tracing::trace!(
                    route.group = %policy.meta.group(),
                    route.kind = %policy.meta.kind(),
                    route.name = %policy.meta.name(),
                    "req.match" = ?req_match,
                    "Using HTTPRoute service",
                );

                self.services.get(policy).expect("route must exist").clone()
            }
            None => {
                tracing::trace!("No HTTPRoutes matched");
                self.default.clone()
            }
        };

        inner.oneshot(req).map_err(Into::into)
    }
}

async fn changed(mut rx: Receiver) -> Result<Receiver, Error> {
    rx.changed()
        .await
        .map_err(|_| anyhow::anyhow!("client policy watch has closed"))?;
    Ok(rx)
}
