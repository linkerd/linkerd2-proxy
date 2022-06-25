pub mod r#match;

pub use self::r#match::MatchRoute;
#[cfg(test)]
mod tests;

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
