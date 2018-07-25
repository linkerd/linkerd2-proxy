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
) -> (Sensors, metrics::Serve) {
    let (metrics_record, metrics_serve) = metrics::new(process, metrics_retain_idle);
    let s = Sensors::new(metrics_record, taps);
    (s, metrics_serve)
}
