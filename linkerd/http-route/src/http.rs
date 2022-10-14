pub mod filter;
pub mod r#match;
#[cfg(test)]
mod tests;

pub use self::r#match::{HostMatch, MatchHeader, MatchHost, MatchPath, MatchRequest};

pub type RouteMatch = crate::RouteMatch<r#match::RequestMatch>;

pub type Route<P> = crate::Route<MatchRequest, P>;

pub type Rule<P> = crate::Rule<MatchRequest, P>;

#[inline]
pub fn find<'r, P, B>(
    routes: &'r [Route<P>],
    req: &::http::Request<B>,
) -> Option<(RouteMatch, &'r P)> {
    super::find(routes, req)
}
