use crate::zone::TcpZoneMetrics;

#[derive(Clone, Debug)]
pub struct ZoneMetricsSensor {
    pub metrics: TcpZoneMetrics,
}

pub type ZoneSensorIo<T> = linkerd_io::SensorIo<T, ZoneMetricsSensor>;

impl linkerd_io::Sensor for ZoneMetricsSensor {
    fn record_read(&mut self, sz: usize) {
        self.metrics.recv_bytes.inc_by(sz as u64);
    }

    fn record_write(&mut self, sz: usize) {
        self.metrics.send_bytes.inc_by(sz as u64);
    }

    fn record_close(&mut self, _eos: Option<linkerd_errno::Errno>) {}

    fn record_error<T>(&mut self, op: linkerd_io::Poll<T>) -> linkerd_io::Poll<T> {
        op
    }
}
