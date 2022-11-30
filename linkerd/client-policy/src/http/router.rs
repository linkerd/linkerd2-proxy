use super::{Route, RoutePolicy};
use crate::Receiver;
use futures::FutureExt;
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
use tokio::sync::watch;
use tokio_util::sync::ReusableBoxFuture;

pub struct ServiceRouter<T, N, S> {
    new_route: N,
    target: T,
    changed: ReusableBoxFuture<'static, Result<Receiver, watch::error::RecvError>>,
    http_routes: Arc<[Route]>,
    services: HashMap<RoutePolicy, S>,
    default: S,
}

// === impl ServiceRouter ===

impl<B, T, N, S> Service<http::Request<B>> for ServiceRouter<T, N, S>
where
    T: Clone,
    N: NewService<(Option<RoutePolicy>, T), Service = S> + Clone,
    S: Service<http::Request<B>, Error = Error> + Clone,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Oneshot<S, http::Request<B>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut policy = match self.changed.poll_unpin(cx) {
            Poll::Pending => return Poll::Ready(Ok(())),
            Poll::Ready(Err(_)) => {
                return Poll::Ready(Err(anyhow::anyhow!("client policy watch has closed").into()))
            }
            Poll::Ready(Ok(policy)) => policy,
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
        self.changed.set(async move {
            policy.changed().await?;
            Ok(policy)
        });

        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        todo!("eliza")
    }
}
