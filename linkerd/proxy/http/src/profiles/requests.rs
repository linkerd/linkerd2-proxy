use super::{RequestMatch, Route, WithRoute};
use linkerd2_stack::{NewService, Proxy};
use tracing::trace;

/// A proxy that applies per-request "routes" over a common inner service.
#[derive(Clone, Debug, Default)]
pub struct Requests<T: WithRoute, M: NewService<T::Route>> {
    target: T,
    make: M,
    default: M::Service,
    routes: Vec<(RequestMatch, M::Service)>,
}

impl<T, M> Requests<T, M>
where
    T: Clone + WithRoute,
    M: NewService<T::Route>,
{
    pub fn new(target: T, make: M, default: Route) -> Self {
        let default = {
            let t = target.clone().with_route(default);
            make.new_service(t)
        };
        Self {
            target,
            make,
            default,
            routes: Vec::default(),
        }
    }

    pub fn set_routes(&mut self, routes: Vec<(RequestMatch, Route)>) {
        self.routes = routes
            .into_iter()
            .map(|(cond, r)| {
                let t = self.target.clone().with_route(r);
                (cond, self.make.new_service(t))
            })
            .collect();
    }
}

impl<T, M, P, B, S> Proxy<http::Request<B>, S> for Requests<T, M>
where
    T: WithRoute,
    M: NewService<T::Route, Service = P>,
    P: Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, inner: &mut S, req: http::Request<B>) -> Self::Future {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!(?condition, "using configured route");
                return route.proxy(inner, req);
            }
        }

        self.default.proxy(inner, req)
    }
}
