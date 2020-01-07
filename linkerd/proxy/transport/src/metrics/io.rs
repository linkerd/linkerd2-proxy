use super::Sensor;
use bytes::Buf;
use futures::{try_ready, Async, Poll};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

/// Wraps a transport with telemetry.
#[derive(Debug)]
pub struct Io<T> {
    io: T,
    sensor: Sensor,
}

// === impl Io ===

impl<T> Io<T> {
    pub(super) fn new(io: T, sensor: Sensor) -> Self {
        Self { io, sensor }
    }

    /// Wraps an operation on the underlying transport with error telemetry.
    ///
    /// If the transport operation results in a non-recoverable error, record a
    /// transport closure.
    fn sense_err<F, U>(&mut self, op: F) -> io::Result<U>
    where
        F: FnOnce(&mut T) -> io::Result<U>,
    {
        match op(&mut self.io) {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    let eos = e.raw_os_error().map(|e| e.into());
                    self.sensor.record_close(eos);
                }

                Err(e)
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite> io::Read for Io<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes = self.sense_err(move |io| io.read(buf))?;
        self.sensor.record_read(bytes);

        Ok(bytes)
    }
}

impl<T: AsyncRead + AsyncWrite> io::Write for Io<T> {
    fn flush(&mut self) -> io::Result<()> {
        self.sense_err(|io| io.flush())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes = self.sense_err(move |io| io.write(buf))?;
        self.sensor.record_write(bytes);

        Ok(bytes)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncRead for Io<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncWrite for Io<T> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        tracing::trace!("Shutting down write-side");
        self.sense_err(|io| io.shutdown())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        let bytes = try_ready!(self.sense_err(|io| io.write_buf(buf)));
        self.sensor.record_write(bytes);

        Ok(Async::Ready(bytes))
    }
}
