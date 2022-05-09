use linkerd_metrics::{metrics, Counter, FmtMetrics, Gauge, MillisAsSeconds};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

metrics! {
    process_start_time_seconds: Gauge {
        "Time that the process started (in seconds since the UNIX epoch)"
    },

    process_uptime_seconds_total: Counter<MillisAsSeconds> {
        "Total time since the process started (in seconds)"
    }
}

/// The process' start time, consisting of both a `SystemTime` *and* an
/// `Instant`.
///
/// The `SystemTime` represents the Unix timestamp when the process
/// started, while the `Instant` is anchored against that unix timestamp and is
/// used to calculate elapsed time more efficiently than by taking system timestamps.
#[derive(Copy, Clone, Debug)]
pub struct StartTime {
    sys: SystemTime,
    instant: Instant,
}

#[derive(Clone, Debug)]
pub struct Report {
    /// The process's start time in seconds since the Unix epoch.
    ///
    /// This is used to generate the `pprocess_start_time_seconds` gauge. This
    /// could be calculated from the `SystemTime``, but the value will never change,
    /// so we may as well pre-calculate it once.
    start_time_from_epoch: u64,

    /// The process's start time as an `Instant`, used for calculating the uptime.
    start_instant: Instant,

    #[cfg(target_os = "linux")]
    system: linux::System,
}

impl Report {
    pub fn new(start_time: StartTime) -> Self {
        let start_time_from_epoch = start_time
            .sys
            .duration_since(UNIX_EPOCH)
            .expect("process start time")
            .as_secs();

        #[cfg(not(target_os = "linux"))]
        tracing::info!("System-level metrics are only supported on Linux");
        Self {
            start_time_from_epoch,
            start_instant: start_time.instant,
            #[cfg(target_os = "linux")]
            system: linux::System::new(),
        }
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        process_start_time_seconds.fmt_help(f)?;
        process_start_time_seconds.fmt_metric(f, &Gauge::from(self.start_time_from_epoch))?;

        //  Use `staturating_duration_since` rather than `elapsed` to avoid
        //  possible panics in the event of clock non-monotonicity.
        let uptime = Instant::now().saturating_duration_since(self.start_instant);
        let uptime_millis = uptime.as_millis();
        process_uptime_seconds_total.fmt_help(f)?;
        process_uptime_seconds_total.fmt_metric(f, &Counter::from(uptime_millis as u64))?;

        #[cfg(target_os = "linux")]
        self.system.fmt_metrics(f)?;

        Ok(())
    }
}

impl StartTime {
    pub fn now() -> Self {
        Self {
            sys: SystemTime::now(),
            instant: Instant::now(),
        }
    }
}

impl From<StartTime> for Instant {
    fn from(StartTime { instant, .. }: StartTime) -> Instant {
        instant
    }
}

impl Default for StartTime {
    fn default() -> Self {
        Self::now()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use linkerd_metrics::{metrics, Counter, FmtMetrics, Gauge, MillisAsSeconds};
    use linkerd_system as sys;
    use std::fmt;
    use tracing::warn;

    metrics! {
        process_cpu_seconds_total: Counter<MillisAsSeconds> {
            "Total user and system CPU time spent in seconds."
        },
        process_open_fds: Gauge { "Number of open file descriptors." },
        process_max_fds: Gauge { "Maximum number of open file descriptors." },
        process_virtual_memory_bytes: Gauge {
            "Virtual memory size in bytes."
        },
        process_resident_memory_bytes: Gauge {
            "Resident memory size in bytes."
        }
    }

    #[derive(Clone, Debug, Default)]
    pub(super) struct System {
        page_size: Option<u64>,
        ms_per_tick: Option<u64>,
    }

    impl System {
        pub fn new() -> Self {
            let page_size = match sys::page_size() {
                Ok(ps) => Some(ps),
                Err(err) => {
                    warn!("Failed to load page size: {}", err);
                    None
                }
            };
            let ms_per_tick = match sys::ms_per_tick() {
                Ok(mpt) => Some(mpt),
                Err(err) => {
                    warn!("Failed to load cpu clock speed: {}", err);
                    None
                }
            };
            Self {
                page_size,
                ms_per_tick,
            }
        }
    }

    impl FmtMetrics for System {
        fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let stat = match sys::blocking_stat() {
                Ok(stat) => stat,
                Err(err) => {
                    warn!("failed to read process stats: {}", err);
                    return Ok(());
                }
            };

            if let Some(mpt) = self.ms_per_tick {
                let clock_ticks = stat.utime as u64 + stat.stime as u64;
                let cpu_ms = clock_ticks * mpt;
                process_cpu_seconds_total.fmt_help(f)?;
                process_cpu_seconds_total.fmt_metric(f, &Counter::from(cpu_ms))?;
            } else {
                warn!("Could not determine process_cpu_seconds_total");
            }

            process_virtual_memory_bytes.fmt_help(f)?;
            process_virtual_memory_bytes.fmt_metric(f, &Gauge::from(stat.vsize as u64))?;

            if let Some(ps) = self.page_size {
                process_resident_memory_bytes.fmt_help(f)?;
                process_resident_memory_bytes.fmt_metric(f, &Gauge::from(stat.rss as u64 * ps))?;
            } else {
                warn!("Could not determine process_resident_memory_bytes");
            }

            match sys::open_fds(stat.pid) {
                Ok(open_fds) => {
                    process_open_fds.fmt_help(f)?;
                    process_open_fds.fmt_metric(f, &open_fds.into())?;
                }
                Err(err) => {
                    warn!("Could not determine process_open_fds: {}", err);
                }
            }

            match sys::max_fds() {
                Ok(None) => {}
                Ok(Some(max_fds)) => {
                    process_max_fds.fmt_help(f)?;
                    process_max_fds.fmt_metric(f, &max_fds.into())?;
                }
                Err(err) => {
                    warn!("Could not determine process_max_fds: {}", err);
                }
            }

            Ok(())
        }
    }
}
