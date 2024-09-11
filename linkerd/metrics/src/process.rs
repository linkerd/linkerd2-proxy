use crate::prom::{self, encoding::EncodeMetric};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

pub fn register(reg: &mut prom::Registry) {
    let start_time = Instant::now();
    let start_time_from_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("process start time");

    reg.register_with_unit(
        "start_time",
        "Time that the process started (in seconds since the UNIX epoch)",
        prom::Unit::Seconds,
        prom::ConstGauge::new(start_time_from_epoch.as_secs_f64()),
    );

    let clock_time_ts = prom::Gauge::<f64, ClockMetric>::default();
    reg.register_with_unit(
        "clock_time",
        "Current system time for this proxy",
        prom::Unit::Seconds,
        clock_time_ts,
    );

    reg.register_collector(Box::new(ProcessCollector {
        start_time,
        #[cfg(target_os = "linux")]
        system: linux::System::new(),
    }));

    tracing::debug!("Process metrics registered");

    #[cfg(not(target_os = "linux"))]
    tracing::debug!("System-level process metrics are only supported on Linux");
}

#[derive(Debug)]
struct ProcessCollector {
    start_time: Instant,
    #[cfg(target_os = "linux")]
    system: linux::System,
}

impl prom::collector::Collector for ProcessCollector {
    fn encode(&self, mut encoder: prom::encoding::DescriptorEncoder<'_>) -> std::fmt::Result {
        let uptime = prom::ConstCounter::new(
            Instant::now()
                .saturating_duration_since(self.start_time)
                .as_secs_f64(),
        );
        let ue = encoder.encode_descriptor(
            "uptime",
            "Total time since the process started (in seconds)",
            Some(&prom::Unit::Seconds),
            prom::metrics::MetricType::Counter,
        )?;
        uptime.encode(ue)?;

        #[cfg(target_os = "linux")]
        self.system.encode(encoder)?;

        Ok(())
    }
}

// Metric that always reports the current system time on a call to [`get`].
#[derive(Copy, Clone, Debug, Default)]
struct ClockMetric;

impl prom::GaugeAtomic<f64> for ClockMetric {
    fn inc(&self) -> f64 {
        self.get()
    }

    fn inc_by(&self, _v: f64) -> f64 {
        self.get()
    }

    fn dec(&self) -> f64 {
        self.get()
    }

    fn dec_by(&self, _v: f64) -> f64 {
        self.get()
    }

    fn set(&self, _v: f64) -> f64 {
        self.get()
    }

    fn get(&self) -> f64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_secs_f64().floor(),
            Err(e) => {
                tracing::warn!(
                    "System time is before the UNIX epoch; reporting negative timestamp"
                );
                -e.duration().as_secs_f64().floor()
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::prom::{self, encoding::EncodeMetric};
    use linkerd_system as sys;
    use tokio::time::Duration;

    #[derive(Clone, Debug)]
    pub(super) struct System {
        page_size: Option<u64>,
        ms_per_tick: Option<u64>,
    }

    impl System {
        pub fn new() -> Self {
            let page_size = match sys::page_size() {
                Ok(ps) => Some(ps),
                Err(err) => {
                    tracing::debug!("Failed to load page size: {}", err);
                    None
                }
            };
            let ms_per_tick = match sys::ms_per_tick() {
                Ok(mpt) => Some(mpt),
                Err(err) => {
                    tracing::debug!("Failed to load cpu clock speed: {}", err);
                    None
                }
            };

            Self {
                page_size,
                ms_per_tick,
            }
        }
    }

    impl prom::collector::Collector for System {
        fn encode(&self, mut encoder: prom::encoding::DescriptorEncoder<'_>) -> std::fmt::Result {
            let stat = match sys::blocking_stat() {
                Ok(stat) => stat,
                Err(error) => {
                    tracing::warn!(%error, "Failed to read process stats");
                    return Ok(());
                }
            };

            if let Some(mpt) = self.ms_per_tick {
                let clock_ticks = stat.utime + stat.stime;
                let cpu =
                    prom::ConstCounter::new(Duration::from_millis(clock_ticks * mpt).as_secs_f64());
                let cpue = encoder.encode_descriptor(
                    "cpu",
                    "Total user and system CPU time spent in seconds",
                    Some(&prom::Unit::Seconds),
                    prom::metrics::MetricType::Counter,
                )?;
                cpu.encode(cpue)?;
            } else {
                tracing::debug!("Could not determine CPU usage");
            }

            let vm_bytes = prom::ConstGauge::new(stat.vsize as i64);
            let vme = encoder.encode_descriptor(
                "virtual_memory",
                "Virtual memory size in bytes",
                Some(&prom::Unit::Bytes),
                prom::metrics::MetricType::Gauge,
            )?;
            vm_bytes.encode(vme)?;

            if let Some(ps) = self.page_size {
                let rss_bytes = prom::ConstGauge::new((stat.rss * ps) as i64);
                let rsse = encoder.encode_descriptor(
                    "resident_memory",
                    "Resident memory size in bytes",
                    Some(&prom::Unit::Bytes),
                    prom::metrics::MetricType::Gauge,
                )?;
                rss_bytes.encode(rsse)?;
            } else {
                tracing::debug!("Could not determine RSS");
            }

            match sys::open_fds(stat.pid) {
                Ok(open_fds) => {
                    let fds = prom::ConstGauge::new(open_fds as i64);
                    let fdse = encoder.encode_descriptor(
                        "open_fds",
                        "Number of open file descriptors",
                        None,
                        prom::metrics::MetricType::Gauge,
                    )?;
                    fds.encode(fdse)?;
                }
                Err(error) => {
                    tracing::warn!(%error, "Could not determine open fds");
                }
            }

            match sys::max_fds() {
                Ok(max_fds) => {
                    let fds = prom::ConstGauge::new(max_fds as i64);
                    let fdse = encoder.encode_descriptor(
                        "max_fds",
                        "Maximum number of open file descriptors",
                        None,
                        prom::metrics::MetricType::Gauge,
                    )?;
                    fds.encode(fdse)?;
                }
                Err(error) => {
                    tracing::warn!(%error, "Could not determine max fds");
                }
            }

            let threads = prom::ConstGauge::new(stat.num_threads);
            let te = encoder.encode_descriptor(
                "threads",
                "Number of OS threads in the process.",
                None,
                prom::metrics::MetricType::Gauge,
            )?;
            threads.encode(te)?;

            Ok(())
        }
    }
}
