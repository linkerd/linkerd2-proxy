use linkerd_metrics::{metrics, FmtMetrics, Gauge};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(target_os = "linux"))]
use tracing::info;

metrics! {
    process_start_time_seconds: Gauge {
        "Time that the process started (in seconds since the UNIX epoch)"
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    start_time: Arc<Gauge>,

    #[cfg(target_os = "linux")]
    system: linux::System,
}

impl Report {
    pub fn new(start_time: SystemTime) -> Self {
        let t0 = start_time
            .duration_since(UNIX_EPOCH)
            .expect("process start time")
            .as_secs();

        #[cfg(not(target_os = "linux"))]
        info!("System-level metrics are only supported on Linux");
        Self {
            start_time: Arc::new(t0.into()),

            #[cfg(target_os = "linux")]
            system: linux::System::new(),
        }
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        process_start_time_seconds.fmt_help(f)?;
        process_start_time_seconds.fmt_metric(f, self.start_time.as_ref())?;

        #[cfg(target_os = "linux")]
        self.system.fmt_metrics(f)?;

        Ok(())
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
