use std::time::Duration;

use super::histogram::{Bounds, Bucket, Histogram};

/// The maximum value (inclusive) for each latency bucket in
/// milliseconds.
pub const BOUNDS: &Bounds = &Bounds(&[
    Bucket::Le(1.0),
    Bucket::Le(2.0),
    Bucket::Le(3.0),
    Bucket::Le(4.0),
    Bucket::Le(5.0),
    Bucket::Le(10.0),
    Bucket::Le(20.0),
    Bucket::Le(30.0),
    Bucket::Le(40.0),
    Bucket::Le(50.0),
    Bucket::Le(100.0),
    Bucket::Le(200.0),
    Bucket::Le(300.0),
    Bucket::Le(400.0),
    Bucket::Le(500.0),
    Bucket::Le(1_000.0),
    Bucket::Le(2_000.0),
    Bucket::Le(3_000.0),
    Bucket::Le(4_000.0),
    Bucket::Le(5_000.0),
    Bucket::Le(10_000.0),
    Bucket::Le(20_000.0),
    Bucket::Le(30_000.0),
    Bucket::Le(40_000.0),
    Bucket::Le(50_000.0),
    // A final upper bound.
    Bucket::Inf,
]);

/// A duration in milliseconds.
#[derive(Debug, Default, Clone)]
pub struct Ms(Duration);

/// A duration in microseconds.
#[derive(Debug, Default, Clone)]
pub struct Us(Duration);

impl From<Us> for u64 {
    fn from(Us(us): Us) -> u64 {
        us.as_micros().try_into().unwrap_or_else(|_| {
            // These measurements should never be long enough to overflow
            tracing::warn!("Duration::as_micros would overflow u64");
            std::u64::MAX
        })
    }
}

impl From<Duration> for Us {
    fn from(d: Duration) -> Self {
        Us(d)
    }
}

impl Default for Histogram<Us> {
    fn default() -> Self {
        Histogram::new(BOUNDS)
    }
}

impl From<Ms> for u64 {
    fn from(Ms(ms): Ms) -> u64 {
        ms.as_secs()
            .saturating_mul(1_000)
            .saturating_add(u64::from(ms.subsec_nanos()) / 1_000_000)
    }
}

impl From<Duration> for Ms {
    fn from(d: Duration) -> Self {
        Ms(d)
    }
}

impl Default for Histogram<Ms> {
    fn default() -> Self {
        Histogram::new(BOUNDS)
    }
}
