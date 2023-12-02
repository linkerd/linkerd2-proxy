use crate::prom;
use tokio::time;

pub struct Uptime {
    metric: prom::Gauge,
    start: time::Instant,
}

impl Default for Uptime {
    fn default() -> Self {
        Self {
            start: time::Instant::now(),
            metric: prom::Gauge::new(
                "process_uptime_seconds_total",
                "Total time since the process started (in seconds)",
            )
            .expect("valid metric"),
        }
    }
}

impl prom::core::Collector for Uptime {
    fn desc(&self) -> Vec<&prom::core::Desc> {
        self.metric.desc()
    }

    fn collect(&self) -> Vec<prom::proto::MetricFamily> {
        self.metric
            .set((time::Instant::now() - self.start).as_secs_f64());
        self.metric.collect()
    }
}
