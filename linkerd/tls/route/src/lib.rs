//! An TLS route matching library for Linkerd to support the TLSRoute
//! Kubernetes Gateway API types.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_tls::ServerName;
use tracing::trace;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SessionInfo {
    pub sni: ServerName,
}

pub mod sni;
pub use self::sni::{InvalidSni, MatchSni, SniMatch};

/// Groups routing rules under a common set of SNIs.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<P> {
    pub snis: Vec<MatchSni>,
    pub policy: P,
}

/// Summarizes a matched route so that route matches may be compared/ordered. A
/// greater match is preferred over a lesser match.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteMatch {
    sni: SniMatch,
}

pub fn find<P>(routes: &[Route<P>], session_info: SessionInfo) -> Option<(RouteMatch, &P)> {
    trace!(routes = ?routes.len(), "Finding matching route");

    best(routes.iter().filter_map(|rt| {
        trace!(snis = ?rt.snis);

        let sni = if rt.snis.is_empty() {
            None
        } else {
            trace!(%session_info.sni, "matching sni");
            rt.snis
                .iter()
                .filter_map(|a| a.summarize_match(session_info.sni.clone()))
                .max()
        }?;

        Some((RouteMatch { sni }, &rt.policy))
    }))
}

#[inline]
fn best<M: Ord, P>(matches: impl Iterator<Item = (M, P)>) -> Option<(M, P)> {
    // This is roughly equivalent to `max_by(...)` but we want to ensure
    // that the first match wins.
    matches.reduce(|(m0, p0), (m1, p1)| if m0 >= m1 { (m0, p0) } else { (m1, p1) })
}
