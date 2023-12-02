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

#[cfg(test)]
mod tests {
    use super::*;
    use prom::core::Collector;
    use tokio::time;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_uptime() {
        let uptime = Uptime::default();

        time::sleep(time::Duration::from_secs(10)).await;

        assert_eq!(
            uptime.collect()[0].get_metric()[0].get_gauge().get_value(),
            10.0,
            "Uptime should increase over time"
        );
    }
}
