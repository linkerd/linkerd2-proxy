use super::{RequestMatch, Route};
use linkerd2_stack::{NewService, Proxy};
use tracing::trace;

/// A proxy that applies per-request "routes" over a common inner service.
#[derive(Clone, Debug, Default)]
pub struct Requests<T, M, P> {
    target: T,
    make: M,
    default: P,
    routes: Vec<(RequestMatch, P)>,
}

impl<T, M> Requests<T, M, M::Service>
where
    T: Clone,
    M: NewService<(T, Route)>,
{
    pub fn new(target: T, make: M, default: Route) -> Self {
        let default = make.new_service((target.clone(), default));
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
                let t = self.target.clone();
                (cond, self.make.new_service((t, r)))
            })
            .collect();
    }
}

impl<T, M, P, B, S> Proxy<http::Request<B>, S> for Requests<T, M, P>
where
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
                trace!(?condition, "Using configured route");
                return route.proxy(inner, req);
            }
        }

        trace!("Using default route");
        self.default.proxy(inner, req)
    }
}
