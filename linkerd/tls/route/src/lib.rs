//! An TLS route matching library for Linkerd to support the TLSRoute
//! Kubernetes Gateway API types.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_tls::ServerName;
use tracing::trace;

pub mod sni;
#[cfg(test)]
mod tests;

pub use self::sni::{InvalidSni, MatchSni, SniMatch};

/// Groups routing rules under a common set of SNIs.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<P> {
    /// A list of SNIs that this route applies to, to be matched against,
    ///
    /// If at least one match is specified, any match may apply for rules to applied.
    /// When no SNI matches are present, all SNIs match.
    pub snis: Vec<MatchSni>,

    pub policy: P,
}

/// Summarizes a matched route so that route matches may be compared/ordered. A
/// greater match is preferred over a lesser match.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct RouteMatch {
    sni: Option<SniMatch>,
}

/// Provides metadata information about a TLS session. For now this contains
/// only the SNI value but further down the line, we could add more metadata
/// if want to support more advanced routing scenarios.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SessionInfo {
    pub sni: ServerName,
}

pub fn find<P>(routes: &[Route<P>], session_info: SessionInfo) -> Option<(RouteMatch, &P)> {
    trace!(routes = ?routes.len(), "Finding matching route");

    best(routes.iter().filter_map(|rt| {
        trace!(snis = ?rt.snis);
        let sni = if rt.snis.is_empty() {
            None
        } else {
            let session_sni = &session_info.sni;
            trace!(%session_sni, "matching sni");
            let sni_match = rt
                .snis
                .iter()
                .filter_map(|a| a.summarize_match(session_sni))
                .max()?;
            Some(sni_match)
        };

        Some((RouteMatch { sni }, &rt.policy))
    }))
}

#[inline]
fn best<M: Ord, P>(matches: impl Iterator<Item = (M, P)>) -> Option<(M, P)> {
    // This is roughly equivalent to `max_by(...)` but we want to ensure
    // that the first match wins.
    matches.reduce(|(m0, p0), (m1, p1)| if m0 >= m1 { (m0, p0) } else { (m1, p1) })
}
