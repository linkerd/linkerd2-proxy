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
pub mod event;
mod metrics;
mod process;
pub mod sensor;
pub mod tap;
pub mod tls_config_reload;
pub mod transport;

use self::errno::Errno;
pub use self::event::Event;
pub use self::metrics::{Serve as ServeMetrics};
pub use self::sensor::Sensors;

pub fn new(
    start_time: SystemTime,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (Sensors, transport::Registry, tls_config_reload::Sensor, ServeMetrics) {
    let process = process::Report::new(start_time);
    let (transport_registry, transport_report) = transport::new();
    let (tls_config_sensor, tls_config_fmt) = tls_config_reload::new();

    let (record, serve) = metrics::new(
        metrics_retain_idle,
        process,
        transport_report,
        tls_config_fmt
    );
    let s = Sensors::new(record, taps);
    (s, transport_registry, tls_config_sensor, serve)
}
