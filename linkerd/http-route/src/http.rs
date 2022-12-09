pub mod filter;
pub mod r#match;
#[cfg(test)]
mod tests;

pub use self::r#match::{HostMatch, MatchHeader, MatchHost, MatchRequest};

pub type RouteMatch = crate::RouteMatch<r#match::RequestMatch>;

pub type Route<P> = crate::Route<MatchRequest, P>;

pub type Rule<P> = crate::Rule<MatchRequest, P>;

#[derive(Debug, thiserror::Error, Default)]
#[error("no route found for request")]
pub struct HttpRouteNotFound(());

#[inline]
pub fn find<'r, P, B>(
    routes: &'r [Route<P>],
    req: &::http::Request<B>,
) -> Option<(RouteMatch, &'r P)> {
    super::find(routes, req)
}
