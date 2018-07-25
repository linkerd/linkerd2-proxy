use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_mpsc_lossy;

use ctx;

mod control;
pub mod event;
pub mod metrics;
pub mod sensor;
pub mod tap;

pub use self::control::{Control, MakeControl};
pub use self::event::Event;
pub use self::sensor::Sensors;

/// Creates proxy-specific runtime telemetry.
///
/// [`Sensors`] hide the details of how telemetry is recorded, but expose proxy utilities
/// that support telemetry.
///
/// [`Control`] drives processing of all telemetry events for tapping as well as metrics
/// aggregation.
///
/// # Arguments
/// - `capacity`: the size of the event queue.
///
/// [`Sensors`]: struct.Sensors.html
/// [`Control`]: struct.Control.html
pub fn new(
    process: &Arc<ctx::Process>,
    metrics_retain_idle: Duration,
    taps: &Arc<Mutex<tap::Taps>>,
) -> (Sensors, metrics::Serve) {
    let (metrics_record, metrics_serve) = metrics::new(process, metrics_retain_idle);
    let s = Sensors::new(metrics_record);
    (s, metrics_serve)
}
