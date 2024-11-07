//! An TCP route matching library for Linkerd to support the TCPRoute
//! Kubernetes Gateway API types.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

/// Groups routing rules under a common set of SNIs.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Route<P> {
    /// Must not be empty.
    pub policy: P,
}

/// Policies for a given set of route matches.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Rule<P> {
    pub policy: P,
}
