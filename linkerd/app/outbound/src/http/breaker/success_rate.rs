//! Time-windowed exact success ratio for the circuit breaker.
//!
//! The breaker's success-rate dimension measures what fraction of the recent
//! responses kept the endpoint healthy, and this module computes it with a small
//! ring of fixed-duration buckets that together span the configured window.
//! Each bucket counts successes and totals for the responses that land in its
//! time slice, so the ratio is the exact quotient of the live buckets and does
//! not depend on how fast requests arrive.
//!
//! A rate-independent ratio matters: a burst of failures at a high request rate
//! must reach the same trip decision as the same burst spread out. An average that
//! weights each sample by inter-arrival time cannot promise that, since a tight
//! burst gives each sample almost no weight and can hide a total outage. Exact
//! counts over a fixed window have no such blind spot.
//!
//! ## Window mechanics
//!
//! The window holds `BUCKETS` slices, each `bucket_width` wide, covering `window`.
//! Recording a response first advances the ring to `now` so any slice whose time
//! has fully passed out of the window is zeroed before the new sample lands, and
//! a gap longer than the whole window clears every bucket so an idle endpoint
//! starts cold. The sample then increments the current bucket's total, and its
//! success count when the response did not degrade the success rate.
//!
//! `BUCKETS` is small, so a check sums all of them rather than tracking a running
//! aggregate. The sum is cheap and keeps the rolling logic easy to read.

use tokio::time::{Duration, Instant};

/// Number of buckets spanning the window. A fixed internal constant: ten slices
/// give the ratio enough time resolution to expire old samples smoothly without
/// making the per-response roll or the per-check sum expensive.
const BUCKETS: usize = 10;

/// Floor for a single bucket's width. With a very short window the per-bucket
/// width could round to zero, which would make ring advancement divide by zero.
/// One millisecond matches the moving average's own window floor.
const MIN_BUCKET_WIDTH: Duration = Duration::from_millis(1);

/// Floor for the whole window, the smallest span that still gives one floored
/// bucket per slice. The window is `bucket_width * BUCKETS`, so flooring `window`
/// here rather than flooring each bucket keeps the realized window equal to the
/// configured `window` for any value at or above this floor. Flooring the bucket
/// width alone would instead round a window between this floor and one bucket up
/// to this span without the caller asking for it.
const MIN_WINDOW: Duration =
    Duration::from_millis(MIN_BUCKET_WIDTH.as_millis() as u64 * BUCKETS as u64);

/// One slice of the success-rate window.
#[derive(Clone, Copy, Debug, Default)]
struct Bucket {
    successes: u32,
    total: u32,
}

impl Bucket {
    fn clear(&mut self) {
        self.successes = 0;
        self.total = 0;
    }
}

/// A ring of fixed-duration buckets tracking the exact success ratio over a
/// trailing `window`.
///
/// Construct one per active breaker with [`SuccessRateWindow::new`], feed each
/// classified response to [`record`](Self::record), and ask
/// [`should_trip`](Self::should_trip) whether the ratio has fallen far enough to
/// open the circuit, while [`reset`](Self::reset) clears every bucket so a
/// recovered endpoint starts cold.
#[derive(Debug)]
pub(crate) struct SuccessRateWindow {
    buckets: [Bucket; BUCKETS],
    /// Width of one bucket. `window / BUCKETS`, floored at [`MIN_BUCKET_WIDTH`].
    bucket_width: Duration,
    /// Index of the bucket that `last_roll` belongs to.
    current: usize,
    /// Start instant of the current bucket's slice. Advancing the ring moves
    /// this forward in whole `bucket_width` steps.
    last_roll: Instant,
}

impl SuccessRateWindow {
    /// Build a window spanning `window`, anchored at `now`.
    ///
    /// The realized window is `bucket_width * BUCKETS`, so the window is floored at
    /// [`MIN_WINDOW`] before the division rather than flooring each bucket. The
    /// realized window then equals the configured `window` exactly for any value at
    /// or above the floor, and the floor keeps the bucket width non-zero for a
    /// shorter window the config layer would otherwise reject.
    pub(crate) fn new(window: Duration, now: Instant) -> Self {
        let bucket_width = window.max(MIN_WINDOW) / BUCKETS as u32;
        Self {
            buckets: [Bucket::default(); BUCKETS],
            bucket_width,
            current: 0,
            last_roll: now,
        }
    }

    /// Advance the ring so the current bucket covers `now`, zeroing any slices
    /// whose time has passed out of the window.
    ///
    /// When more than `BUCKETS` widths have elapsed the whole ring is too old to
    /// keep, so every bucket is cleared at once rather than stepped one at a time.
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_roll);
        // Integer division: how many whole bucket widths have passed since the
        // current slice began. Zero means the sample belongs to the live bucket.
        let steps = (elapsed.as_nanos() / self.bucket_width.as_nanos().max(1)) as usize;
        if steps == 0 {
            return;
        }

        if steps >= BUCKETS {
            for b in &mut self.buckets {
                b.clear();
            }
        } else {
            // Clear the slices we are moving onto. Walking forward one step at
            // a time and zeroing each landing bucket drops exactly the slices
            // that have aged out of the trailing window.
            for i in 1..=steps {
                let idx = (self.current + i) % BUCKETS;
                self.buckets[idx].clear();
            }
        }

        self.current = (self.current + steps) % BUCKETS;
        // Anchor the new slice on a bucket-width boundary so fractional time does
        // not accumulate across rolls. Advancing by the elapsed time less its
        // sub-width remainder steps forward `steps` widths and stays exact even
        // across an idle gap wider than `u32::MAX` widths. A `steps as u32` multiply
        // would instead truncate and settle the anchor far behind `now`, re-clearing
        // every bucket on each later roll and pinning the total at one.
        let remainder = elapsed.as_nanos() % self.bucket_width.as_nanos().max(1);
        self.last_roll += elapsed - Duration::from_nanos(remainder as u64);
    }

    /// Record one classified response. `success` is false when the response
    /// degraded the success rate (the same determination the breaker applies to
    /// 5xx, 429, and gRPC RESOURCE_EXHAUSTED).
    pub(crate) fn record(&mut self, success: bool, now: Instant) {
        self.roll(now);
        let bucket = &mut self.buckets[self.current];
        bucket.total = bucket.total.saturating_add(1);
        if success {
            bucket.successes = bucket.successes.saturating_add(1);
        }
    }

    /// Sum the buckets into `(successes, total)`.
    ///
    /// This reads the buckets as of the last [`roll`](Self::roll) and does not
    /// roll itself, so a caller that has not `record`ed (which rolls) or `reset`
    /// since the window aged sees old slices. The breaker records each response
    /// before it checks the ratio, so its own reads are always current.
    fn totals(&self) -> (u64, u64) {
        let mut successes = 0u64;
        let mut total = 0u64;
        for b in &self.buckets {
            successes += u64::from(b.successes);
            total += u64::from(b.total);
        }
        (successes, total)
    }

    /// Window success ratio paired with the sample count, computed in one place.
    /// The ratio is `None` exactly when the window holds no samples.
    fn ratio_and_total(&self) -> (Option<f64>, u64) {
        let (successes, total) = self.totals();
        let ratio = (total != 0).then(|| successes as f64 / total as f64);
        (ratio, total)
    }

    /// Current in-window success ratio, or `None` when the window holds no
    /// samples. Callers that need a trip decision should prefer
    /// [`should_trip`](Self::should_trip), which folds in the sample-count guard.
    pub(crate) fn ratio(&self) -> Option<f64> {
        self.ratio_and_total().0
    }

    /// Whether the in-window ratio has fallen below `threshold` with enough
    /// samples to act on.
    ///
    /// The `total > 0` guard makes `min_requests == 0` safe because with no
    /// samples the check is skipped. A zero floor then acts like a floor of one
    /// and never divides by zero. A `min_requests` of `usize::MAX` can never be
    /// reached, so it keeps this dimension dormant.
    pub(crate) fn should_trip(&self, min_requests: usize, threshold: f64) -> bool {
        let (ratio, total) = self.ratio_and_total();
        match ratio {
            Some(r) => (total as usize) >= min_requests && r < threshold,
            None => false,
        }
    }

    /// Clear every bucket so the window starts cold. Recovery calls this so a
    /// reopened endpoint must re-accumulate evidence before it can trip again.
    pub(crate) fn reset(&mut self, now: Instant) {
        for b in &mut self.buckets {
            b.clear();
        }
        self.current = 0;
        self.last_roll = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{self, Duration, Instant};

    fn window_10s() -> (SuccessRateWindow, Instant) {
        let now = Instant::now();
        (SuccessRateWindow::new(Duration::from_secs(10), now), now)
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bucket_width_floors_at_minimum() {
        let now = Instant::now();
        // A sub-millisecond window would divide to a zero-width bucket, so the floor
        // keeps the ring usable.
        let w = SuccessRateWindow::new(Duration::from_micros(100), now);
        assert_eq!(w.bucket_width, MIN_BUCKET_WIDTH);
    }

    // The realized window is `bucket_width * BUCKETS`, and flooring the window
    // rather than the bucket keeps it equal to the configured window for any value
    // at or above the floor. A 5s window realizes a 5s window exactly, and a window
    // at the floor realizes the floor rather than rounding up.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn realized_window_equals_configured_window() {
        let now = Instant::now();

        let window = Duration::from_secs(5);
        let w = SuccessRateWindow::new(window, now);
        assert_eq!(
            w.bucket_width * BUCKETS as u32,
            window,
            "a window above the floor realizes a window of exactly that window",
        );

        // At the floor the realized window is the floor, not a wider span.
        let w = SuccessRateWindow::new(MIN_WINDOW, now);
        assert_eq!(w.bucket_width * BUCKETS as u32, MIN_WINDOW);

        // Below the floor the window is held at the floor rather than widening to
        // some larger value as flooring the bucket width alone would.
        let w = SuccessRateWindow::new(Duration::from_millis(1), now);
        assert_eq!(w.bucket_width * BUCKETS as u32, MIN_WINDOW);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn empty_window_does_not_trip() {
        let (w, _now) = window_10s();
        assert_eq!(w.ratio(), None);
        assert!(!w.should_trip(0, 1.0), "no samples must never trip");
        assert!(!w.should_trip(1, 0.5));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn min_requests_zero_trips_on_first_failure() {
        let (mut w, now) = window_10s();
        w.record(false, now);
        // total is 1 (> 0), ratio 0.0 < 0.5: with a zero floor the first failing
        // sample is enough.
        assert!(w.should_trip(0, 0.5));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn threshold_one_trips_on_any_failure_once_min_met() {
        let (mut w, now) = window_10s();
        // Three successes then one failure: ratio 0.75 < 1.0.
        for _ in 0..3 {
            w.record(true, now);
        }
        assert!(!w.should_trip(4, 1.0), "only 3 samples, min_requests is 4");
        w.record(false, now);
        assert!(
            w.should_trip(4, 1.0),
            "zero-tolerance threshold trips on the first failure once min is met",
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cold_start_holds_until_min_requests() {
        let (mut w, now) = window_10s();
        // Four failures, but min_requests is five.
        for _ in 0..4 {
            w.record(false, now);
        }
        assert!(!w.should_trip(5, 0.5), "4 < 5 samples must not trip");
        w.record(false, now);
        assert!(w.should_trip(5, 0.5), "the fifth sample reaches the floor");
    }

    // The rate-independence property: a 100%-failure burst reaches the trip
    // ratio after the same number of samples no matter how tightly packed they
    // are in time. The old time-decay average failed this, since a fast burst
    // gave each sample almost no weight. Here a five-failure burst within a
    // fraction of the window trips exactly when the sample floor is met, and the
    // same burst at a slow rate trips at the same count.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn failure_burst_trips_independent_of_rate() {
        // Fast burst: five failures inside a single millisecond, far shorter
        // than the 10s decay.
        let now = Instant::now();
        let mut fast = SuccessRateWindow::new(Duration::from_secs(10), now);
        for _ in 0..4 {
            fast.record(false, now);
            assert!(
                !fast.should_trip(5, 0.5),
                "must wait for the sample floor even in a tight burst",
            );
        }
        fast.record(false, now);
        assert!(
            fast.should_trip(5, 0.5),
            "a high-rate 100% failure burst trips once min_requests is met",
        );
        assert_eq!(fast.ratio(), Some(0.0));

        // Slow burst: the same five failures, one per second. The window is 10s,
        // so all five stay in the window. The trip lands at the same fifth sample,
        // proving the decision follows the failure ratio and ignores the rate.
        let now2 = Instant::now();
        let mut slow = SuccessRateWindow::new(Duration::from_secs(10), now2);
        for i in 0..4 {
            slow.record(false, now2 + Duration::from_secs(i));
            assert!(!slow.should_trip(5, 0.5));
        }
        slow.record(false, now2 + Duration::from_secs(4));
        assert!(
            slow.should_trip(5, 0.5),
            "the same burst spread over time trips at the same sample count",
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn samples_age_out_after_window() {
        let (mut w, now) = window_10s();
        // Fill the window with failures.
        for _ in 0..5 {
            w.record(false, now);
        }
        assert_eq!(w.ratio(), Some(0.0));
        assert!(w.should_trip(1, 0.5));

        // A response arriving a full window later rolls the ring on record. Every
        // failing bucket ages out, so only the fresh success remains.
        time::advance(Duration::from_secs(11)).await;
        let later = Instant::now();
        w.record(true, later);
        assert_eq!(
            w.ratio(),
            Some(1.0),
            "failures older than the window no longer count",
        );
        assert!(
            !w.should_trip(1, 0.5),
            "with only a fresh success the ratio is back to healthy",
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn ratio_recovers_as_failures_expire() {
        let now = Instant::now();
        let mut w = SuccessRateWindow::new(Duration::from_secs(10), now);
        // Two failures early in the window.
        w.record(false, now);
        w.record(false, now);
        assert_eq!(w.ratio(), Some(0.0));

        // Six seconds later, eight successes. Both samples still live: 8/10.
        let t6 = now + Duration::from_secs(6);
        for _ in 0..8 {
            w.record(true, t6);
        }
        assert_eq!(w.ratio(), Some(0.8));

        // Past the window from the failures, only the successes remain: 8/8.
        time::advance(Duration::from_secs(11)).await;
        w.record(true, Instant::now());
        assert!(
            w.ratio().unwrap() > 0.99,
            "expired failures should no longer drag the ratio down",
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn reset_clears_all_buckets() {
        let (mut w, now) = window_10s();
        for _ in 0..5 {
            w.record(false, now);
        }
        assert!(w.should_trip(1, 0.5));
        w.reset(now);
        assert_eq!(w.ratio(), None, "reset must empty the window");
        assert!(!w.should_trip(1, 0.5));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn unreachable_min_requests_never_trips() {
        let (mut w, now) = window_10s();
        // Even a window full of failures cannot reach a usize::MAX sample floor.
        for _ in 0..50 {
            w.record(false, now);
        }
        assert!(
            !w.should_trip(usize::MAX, 1.0),
            "an unreachable min_requests floor keeps this dimension dormant",
        );
    }

    // Samples land in the bucket for their time slice, and the slice that passes
    // out of the window is the one that gets cleared. Here window=10s gives 1s
    // buckets, so a failure at t=0 and a success at t=5s sit in different buckets,
    // and after 10s the t=0 bucket ages out while the t=5s one survives.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn buckets_roll_and_expire_in_order() {
        let now = Instant::now();
        let mut w = SuccessRateWindow::new(Duration::from_secs(10), now);

        w.record(false, now); // bucket 0
        w.record(true, now + Duration::from_secs(5)); // bucket 5
        assert_eq!(w.ratio(), Some(0.5), "one failure, one success in window");

        // At t=10.5s the t=0 failure has aged out (10s window), the t=5 success
        // has not. Recording a probe read at this instant should see only the
        // success plus the new sample.
        let t = now + Duration::from_millis(10_500);
        w.record(true, t);
        assert_eq!(
            w.ratio(),
            Some(1.0),
            "the oldest failure expired; only successes remain",
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn large_gap_clears_entire_window() {
        let now = Instant::now();
        let mut w = SuccessRateWindow::new(Duration::from_secs(10), now);
        for _ in 0..5 {
            w.record(false, now);
        }
        // A gap of many windows wipes everything, and the next sample starts fresh.
        let far = now + Duration::from_secs(1_000);
        w.record(true, far);
        assert_eq!(w.ratio(), Some(1.0));
    }

    // A very long idle gap must not stop the window from counting once traffic
    // returns. With the smallest bucket width of 1ms the gap reaches about fifty
    // days, and past that point a `steps as u32` cast truncates and leaves
    // `last_roll` 2^32 widths behind `now`. Every later roll would then clear the
    // whole ring, and the total would never grow past one sample.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_gap_beyond_u32_steps_does_not_wedge_window() {
        let now = Instant::now();
        let mut w = SuccessRateWindow::new(Duration::from_millis(1), now);
        assert_eq!(w.bucket_width, MIN_BUCKET_WIDTH);

        w.record(false, now);

        // Sixty days is more than u32::MAX milliseconds, so the first roll past
        // the gap sees a step count that overflows a u32.
        let far = now + Duration::from_secs(60 * 86_400);
        w.record(true, far);
        w.record(false, far + MIN_BUCKET_WIDTH);
        w.record(true, far + MIN_BUCKET_WIDTH * 2);

        assert_eq!(
            w.totals(),
            (2, 3),
            "samples after a gap exceeding u32::MAX steps must accumulate",
        );
    }
}
