use crate::policy::Policy;
use futures::{future::MapErr, FutureExt, TryFutureExt};
use linkerd_client_policy::{http::Route, RoutePolicy};
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
    changed: ReusableBoxFuture<'static, Result<Policy, Error>>,
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
    T: Param<Option<Policy>> + Clone,
    N: NewService<(Option<RoutePolicy>, T)> + Clone,
{
    type Service = ServiceRouter<T, N, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let rx = target
            .param()
            // TODO(eliza): build this with a `(Policy, T)` target instead so we
            // know the policy is there...
            .expect("new service router should only be built when a policy has been discovered");
        let default = self.0.new_service((None, target.clone()));
        let http_routes = rx.policy.borrow().http_routes.clone();
        let mut router = ServiceRouter {
            default,
            target,
            changed: ReusableBoxFuture::new(Box::pin(changed(rx))),
            http_routes: http_routes.clone(),
            services: HashMap::new(),
            new_route: self.0.clone(),
        };
        router.update_route_policies(route_policies(&http_routes));
        router
    }
}

// === impl ServiceRouter ===

impl<T, N, S> ServiceRouter<T, N, S> {
    fn update_route_policies(&mut self, route_policies: impl IntoIterator<Item = RoutePolicy>)
    where
        T: Clone,
        N: NewService<(Option<RoutePolicy>, T), Service = S> + Clone,
    {
        for route in route_policies.into_iter() {
            if let hash_map::Entry::Vacant(ent) = self.services.entry(route) {
                let route = ent.key().clone();
                let svc = self
                    .new_route
                    .new_service((Some(route), self.target.clone()));
                ent.insert(svc);
            }
        }
    }
}

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

        let http_routes = policy.policy.borrow_and_update().http_routes.clone();

        // If the routes have been updated, update the cache.
        tracing::debug!(routes = %http_routes.len(), "Updating client policy HTTP routes");

        // XXX(eliza): this will unify all routes that share the same policy to
        // have a single service...which is nice, unless we want e.g. different
        // metrics for each route. if we want that, we should probably include
        // the metric labels in the policy, i think?
        let route_policies = route_policies(&http_routes).collect::<HashSet<_>>();
        self.http_routes = http_routes;

        // Clear out defunct routes before building any missing routes.
        self.services.retain(|r, _| route_policies.contains(r));
        self.update_route_policies(route_policies);

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

async fn changed(mut policy: Policy) -> Result<Policy, Error> {
    policy
        .policy
        .changed()
        .await
        .map_err(|_| anyhow::anyhow!("client policy watch has closed"))?;
    Ok(policy)
}

fn route_policies(routes: &Arc<[Route]>) -> impl Iterator<Item = RoutePolicy> + '_ {
    routes
        .iter()
        .flat_map(|route| route.rules.iter())
        .map(|rule| rule.policy.clone())
}
