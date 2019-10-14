use super::{RequestMatch, Route, WeightedAddr, WithAddr, WithRoute};
use http;
use linkerd2_router as rt;
use rand::distributions::{Distribution, WeightedIndex};
use std::hash::Hash;
use tracing::trace;

#[derive(Clone)]
pub struct RouteRecognize<T> {
    target: T,
    routes: Vec<(RequestMatch, Route)>,
    default_route: Route,
}

#[derive(Clone)]
pub struct ConcreteDstRecognize<T> {
    target: T,
    dst_overrides: Vec<WeightedAddr>,
    // A weighted index of the `dst_overrides` weights.  This must only be
    // None if `dst_overrides` is empty.
    distribution: Option<WeightedIndex<u32>>,
}

impl<T> RouteRecognize<T> {
    pub fn new(target: T, routes: Vec<(RequestMatch, Route)>, default_route: Route) -> Self {
        RouteRecognize {
            target,
            routes,
            default_route,
        }
    }
}

impl<Body, T> rt::Recognize<http::Request<Body>> for RouteRecognize<T>
where
    T: WithRoute + Clone,
    T::Output: Clone + Eq + Hash,
{
    type Target = T::Output;

    fn recognize(&self, req: &http::Request<Body>) -> Option<Self::Target> {
        for (ref condition, ref route) in &self.routes {
            if condition.is_match(&req) {
                trace!("using configured route: {:?}", condition);
                return Some(self.target.clone().with_route(route.clone()));
            }
        }

        trace!("using default route");
        Some(self.target.clone().with_route(self.default_route.clone()))
    }
}

impl<T> ConcreteDstRecognize<T> {
    pub fn new(target: T, dst_overrides: Vec<WeightedAddr>) -> Self {
        let distribution = Self::make_dist(&dst_overrides);
        ConcreteDstRecognize {
            target,
            dst_overrides,
            distribution,
        }
    }

    fn make_dist(dst_overrides: &Vec<WeightedAddr>) -> Option<WeightedIndex<u32>> {
        let mut weights = dst_overrides.iter().map(|dst| dst.weight).peekable();
        if weights.peek().is_none() {
            // Weights list is empty.
            None
        } else {
            Some(WeightedIndex::new(weights).expect("invalid weight distribution"))
        }
    }
}

impl<Body, T> rt::Recognize<http::Request<Body>> for ConcreteDstRecognize<T>
where
    T: WithAddr + Clone + Eq + Hash,
{
    type Target = T;

    fn recognize(&self, _req: &http::Request<Body>) -> Option<Self::Target> {
        match self.distribution {
            Some(ref distribution) => {
                let mut rng = rand::thread_rng();
                let idx = distribution.sample(&mut rng);
                let addr = self.dst_overrides[idx].addr.clone();
                Some(self.target.clone().with_addr(addr))
            }
            None => Some(self.target.clone()),
        }
    }
}
