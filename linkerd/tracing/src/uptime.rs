use std::{
    fmt,
    time::{Duration, Instant},
};
use tokio_timer::clock;

pub(crate) struct Uptime {
    start_time: Instant,
}

impl Uptime {
    pub(crate) fn starting_now() -> Self {
        Self {
            start_time: clock::now(),
        }
    }

    fn format(d: Duration, w: &mut dyn fmt::Write) -> fmt::Result {
        let micros = d.subsec_nanos() / 1000;
        write!(w, "[{:>6}.{:06}s]", d.as_secs(), micros)
    }
}

impl tracing_subscriber::fmt::time::FormatTime for Uptime {
    fn format_time(&self, w: &mut dyn fmt::Write) -> fmt::Result {
        Self::format(clock::now() - self.start_time, w)
    }
}

#[cfg(test)]
#[test]
fn test_format() {
    fn fmt(d: Duration) -> String {
        let mut buf = String::new();
        Uptime::format(d, &mut buf).expect("must write to buf");
        buf
    }

    // Anything less than a microsecond is truncated.
    assert_eq!(fmt(Duration::new(1, 100)), "[     1.000000s]");

    // Microseconds are properly reported.
    assert_eq!(fmt(Duration::new(1, 2000)), "[     1.000002s]");

    // The maximum subsecond value is handled.
    assert_eq!(
        fmt(Duration::new(1, 0) - Duration::new(0, 1)),
        "[     0.999999s]"
    );

    // Larger times are not truncated.
    assert_eq!(fmt(Duration::new(1234567, 0)), "[1234567.000000s]");
}
