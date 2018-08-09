use std::sync::{Arc, Mutex};
use std::time::Duration;

use ctx;

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
pub mod event;
// TODO this shouldn't need to be public.
pub mod metrics;
mod process;
pub mod sensor;
pub mod tap;
pub mod tls_config_reload;

use self::errno::Errno;
pub use self::event::Event;
pub use self::sensor::Sensors;

pub fn new(
    process: &Arc<ctx::Process>,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (Sensors, tls_config_reload::Sensor, metrics::Serve) {
    let (tls_config_sensor, tls_config_fmt) = tls_config_reload::new();
    let (metrics_record, metrics_serve) = metrics::new(process, metrics_retain_idle, tls_config_fmt);
    let s = Sensors::new(metrics_record, taps);
    (s, tls_config_sensor, metrics_serve)
}
