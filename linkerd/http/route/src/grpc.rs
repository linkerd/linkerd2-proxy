pub mod filter;
pub mod r#match;
#[cfg(test)]
mod tests;

pub use self::r#match::MatchRoute;

pub type RouteMatch = crate::RouteMatch<r#match::RouteMatch>;

pub type Route<P> = crate::Route<MatchRoute, P>;

pub type Rule<P> = crate::Rule<MatchRoute, P>;

#[inline]
pub fn find<'r, P, B>(
    routes: &'r [Route<P>],
    req: &::http::Request<B>,
) -> Option<(RouteMatch, &'r P)> {
    super::find(routes, req)
}
