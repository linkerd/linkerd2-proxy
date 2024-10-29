//! An TLS route matching library for Linkerd to support the TLSRoute
//! Kubernetes Gateway API types.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use r#match::SessionMatch;
use tracing::trace;

pub mod r#match;

/// Groups routing rules under a common set of SNIs.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<P> {
    /// Must not be empty.
    pub policy: P,

    /// Indicates that traffic should not pass through this route
    pub forbidden: bool,
}

/// Policies for a given set of route matches.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Rule<P> {
    pub policy: P,
}

/// Summarizes a matched route so that route matches may be compared/ordered. A
/// greater match is preferred over a lesser match.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct RouteMatch {
    route: r#match::SessionMatch,
}

pub fn find<P>(routes: &[Route<P>]) -> Option<(RouteMatch, &P)> {
    trace!(routes = ?routes.len(), "Finding matching route");
    best(routes.iter().map(|rt| {
        (
            RouteMatch {
                route: SessionMatch::default(),
            },
            &rt.policy,
        )
    }))
}

#[inline]
fn best<M: Ord, P>(matches: impl Iterator<Item = (M, P)>) -> Option<(M, P)> {
    // This is roughly equivalent to `max_by(...)` but we want to ensure
    // that the first match wins.
    matches.reduce(|(m0, p0), (m1, p1)| if m0 >= m1 { (m0, p0) } else { (m1, p1) })
}
