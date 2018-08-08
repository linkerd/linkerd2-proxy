use std::sync::{Arc, Mutex};
use std::time::Duration;

use ctx;

pub mod event;
pub mod metrics;
pub mod sensor;
pub mod tap;

pub use self::event::Event;
pub use self::sensor::Sensors;

pub fn new(
    process: &Arc<ctx::Process>,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (Sensors, metrics::tls_config_reload::Sensor, metrics::Serve) {
    let (tls_config_sensor, tls_config_fmt) = metrics::tls_config_reload::new();
    let (metrics_record, metrics_serve) = metrics::new(process, metrics_retain_idle, tls_config_fmt);
    let s = Sensors::new(metrics_record, taps);
    (s, tls_config_sensor, metrics_serve)
}
