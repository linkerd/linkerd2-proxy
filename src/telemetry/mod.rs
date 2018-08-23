use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

macro_rules! metrics {
    { $( $name:ident : $kind:ty { $help:expr } ),+ } => {
        $(
            #[allow(non_upper_case_globals)]
            const $name: ::telemetry::metrics::Metric<'static, $kind> =
                ::telemetry::metrics::Metric {
                    name: stringify!($name),
                    help: $help,
                    _p: ::std::marker::PhantomData,
                };
        )+
    }
}

mod errno;
pub mod http;
mod metrics;
mod process;
mod report;
pub mod tap;
pub mod tls_config_reload;
pub mod transport;

use self::errno::Errno;
pub use self::http::event::Event;
pub use self::report::Report;
pub use self::http::Sensors;

pub type ServeMetrics = metrics::Serve<Report>;

pub fn new(
    start_time: SystemTime,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (Sensors, transport::Registry, tls_config_reload::Sensor, ServeMetrics) {
    let process = process::Report::new(start_time);
    let (http_sensors, http_report) = http::new(metrics_retain_idle, taps);
    let (transport_registry, transport_report) = transport::new();
    let (tls_config_sensor, tls_config_report) = tls_config_reload::new();

    let report = Report::new(http_report, transport_report, tls_config_report, process);
    (http_sensors, transport_registry, tls_config_sensor, ServeMetrics::new(report))
}
