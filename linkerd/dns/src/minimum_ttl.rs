//! Minimum TTL enforcement.
//!
//! This module contains functions to enforce a lower-bound for TTL's (including negative TTL's
//! used during error recovery) to prevent DNS resolution from spinning in a hot-loop.

use tokio::time::{Duration, Instant};
use tracing::debug;

/// The minimum TTL duration that will be respected.
const MINIMUM_TTL: Duration = Duration::from_secs(5);

/// Apply a lower-bound to the given [`Instant`].
///
/// NB: This enforces a lower-bound for TTL's to prevent DNS resolution from spinning in a
/// hot-loop.
#[tracing::instrument(level = "debug")]
pub fn with_minimum_expiry(valid_until: Instant) -> Instant {
    let minimum = Instant::now() + MINIMUM_TTL;

    // Choose a deadline; if the expiry is too short, fall back to the minimum TTL.
    let deadline = if valid_until >= minimum {
        valid_until
    } else {
        debug!(ttl.min = ?MINIMUM_TTL, "Given TTL too short, using a minimum TTL");
        minimum
    };

    deadline
}

/// Apply a lower-bound to the given [`Duration`].
///
/// NB: This enforces a lower-bound for negative TTL's to prevent DNS resolution error recovery
/// from spinning in a hot-loop.
pub fn with_minimum_duration(ttl: Duration) -> Duration {
    if ttl < MINIMUM_TTL {
        // Choose a deadline; if the expiry is too short, fall back to the minimum TTL.
        debug!(ttl.min = ?MINIMUM_TTL, ?ttl, "Given Negative TTL too short, using a minimum TTL");
        return MINIMUM_TTL;
    }

    ttl
}
