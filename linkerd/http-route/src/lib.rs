//! An HTTP route matching library for Linkerd to support the HTTPRoute (and
//! GRPCRoute) Kubernetes Gateway API types.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use tracing::trace;

pub mod grpc;
pub mod http;

// Matchers used by both HTTP and gRPC routes.
pub use self::http::{HostMatch, MatchHeader, MatchHost};

/// Groups routing rules under a common set of hostnames.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<M, P> {
    /// A list of hostnames that this route applies to, to be matched against,
    /// e.g., the HTTP `host`.
    ///
    /// If at least one match is specified, any match may apply to the request
    /// for rules to applied. When no host matches are present, all hosts match.
    pub hosts: Vec<http::MatchHost>,

    /// Must not be empty.
    pub rules: Vec<Rule<M, P>>,
}

/// Policies for a given set of route matches.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Rule<M, P> {
    /// A list of request matchers, *any* of which may apply.
    ///
    /// The "best" match is used when comparing rules.
    pub matches: Vec<M>,

    /// The policy to apply to requests matched by this rule.
    pub policy: P,
}

/// Summarizes a matched route so that route matches may be compared/ordered. A
/// greater matches is preferred over a lesser match.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteMatch<T> {
    host: Option<http::HostMatch>,
    route: T,
}

/// A strategy for matching a request to a route.
pub trait Match {
    type Summary: Default + Ord;

    fn r#match<B>(&self, req: &::http::Request<B>) -> Option<Self::Summary>;
}

/// Finds the best matching route policy for a request.
pub fn find<'r, M: Match + 'r, P, B>(
    routes: &'r [Route<M, P>],
    req: &::http::Request<B>,
) -> Option<(RouteMatch<M::Summary>, &'r P)> {
    trace!(routes = ?routes.len(), "Finding matching route");

    best(routes.iter().filter_map(|rt| {
        trace!(hosts = ?rt.hosts);
        let host = if rt.hosts.is_empty() {
            None
        } else {
            let uri = req.uri();
            trace!(%uri, "matching host");
            let hm = rt
                .hosts
                .iter()
                .filter_map(|a| a.summarize_match(uri))
                .max()?;
            Some(hm)
        };

        trace!(rules = %rt.rules.len());
        let (route, policy) = best(rt.rules.iter().filter_map(|rule| {
            // If there are no matches in the list, then the rule has an
            // implicit default match.
            if rule.matches.is_empty() {
                trace!("implicit match");
                return Some((M::Summary::default(), &rule.policy));
            }
            // Find the best match to compare against other rules/routes
            // (if any apply). The order/precedence of matches is not
            // relevant.
            let summary = rule.matches.iter().filter_map(|m| m.r#match(req)).max()?;
            trace!("matches!");
            Some((summary, &rule.policy))
        }))?;

        Some((RouteMatch { host, route }, policy))
    }))
}

#[inline]
fn best<M: Ord, P>(matches: impl Iterator<Item = (M, P)>) -> Option<(M, P)> {
    // This is roughly equivalent to `max_by(...)` but we want to ensure
    // that the first match wins.
    matches.reduce(|(m0, p0), (m1, p1)| if m0 < m1 { (m1, p1) } else { (m0, p0) })
}
