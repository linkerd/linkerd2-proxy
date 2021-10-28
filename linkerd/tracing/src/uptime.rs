use std::fmt;
use tokio::time::{Duration, Instant};
use tracing_subscriber::fmt::{format, time::FormatTime};

pub(crate) struct Uptime {
    start_time: Instant,
}

impl Uptime {
    pub(crate) fn starting_now() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }

    fn format(d: Duration, w: &mut impl fmt::Write) -> fmt::Result {
        let micros = d.subsec_micros();
        write!(w, "[{:>6}.{:06}s]", d.as_secs(), micros)
    }
}

impl FormatTime for Uptime {
    fn format_time(&self, w: &mut format::Writer<'_>) -> fmt::Result {
        Self::format(Instant::now() - self.start_time, w)
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
