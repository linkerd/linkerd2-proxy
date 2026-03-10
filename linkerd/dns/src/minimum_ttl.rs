//! Minimum TTL enforcement.
//!
//! This module contains functions to enforce a lower-bound for TTL's (including negative TTL's
//! used during error recovery) to prevent DNS resolution from spinning in a hot-loop.

use tokio::time::{sleep_until, Duration, Instant};
use tracing::debug;

/// The minimum TTL duration that will be respected.
const MINIMUM_TTL: Duration = Duration::from_secs(5);

/// Sleep for the provided [`Duration`][tokio::time::Duration].
///
/// NB: This enforces a lower-bound for TTL's to prevent DNS resolution from spinning in a
/// hot-loop.
#[tracing::instrument(level = "debug")]
pub async fn sleep_until_expired(valid_until: Instant) {
    let minimum = Instant::now() + MINIMUM_TTL;

    // Choose a deadline; if the expiry is too short, fall back to the minimum TTL.
    let deadline = if valid_until >= minimum {
        valid_until
    } else {
        debug!(ttl.min = ?MINIMUM_TTL, "Given TTL too short, using a minimum TTL");
        minimum
    };

    sleep_until(deadline).await;
}
