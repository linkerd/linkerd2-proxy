use std::time::Duration;

/// The maximum value (inclusive) for each latency bucket in milliseconds.
pub const MILLIS_BOUNDS: &[f64] = &[
    1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0, 100.0, 200.0, 300.0, 400.0, 500.0,
    1_000.0, 2_000.0, 3_000.0, 4_000.0, 5_000.0, 10_000.0, 20_000.0, 30_000.0, 40_000.0, 50_000.0,
];

pub fn millis_u64(dur: Duration) -> u64 {
    crate::value::u128_to_u64(dur.as_millis())
}
