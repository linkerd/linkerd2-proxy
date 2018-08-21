use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

macro_rules! metrics {
    { $( $name:ident : $kind:ty { $help:expr } ),+ } => {
        $(
            #[allow(non_upper_case_globals)]
            const $name: ::telemetry::metrics::prom::Metric<'static, $kind> =
                ::telemetry::metrics::prom::Metric {
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
pub mod tap;
pub mod tls_config_reload;
pub mod transport;

use self::errno::Errno;
pub use self::metrics::{Serve as ServeMetrics};

pub fn new(
    start_time: SystemTime,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (http::Sensors, transport::Registry, tls_config_reload::Sensor, ServeMetrics) {
    let process = process::Report::new(start_time);
    let (http_sensors, http_report) = http::new(taps);
    let (transport_registry, transport_report) = transport::new();
    let (tls_config_sensor, tls_config_fmt) = tls_config_reload::new();

    let report = Arc::new(Mutex::new(metrics::Root::new(
        http_report,
        transport_report,
        tls_config_fmt,
        process,
    )));
    let serve = ServeMetrics::new(&report, metrics_retain_idle);
    (http_sensors, transport_registry, tls_config_sensor, serve)
}
