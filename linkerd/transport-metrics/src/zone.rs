use linkerd_metrics::prom;

pub use self::sensor::ZoneSensorIo;

pub mod client;
mod sensor;

#[derive(Clone, Debug)]
pub struct TcpZoneMetrics {
    pub recv_bytes: prom::Counter,
    pub send_bytes: prom::Counter,
}
