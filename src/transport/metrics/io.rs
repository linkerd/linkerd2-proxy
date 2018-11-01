use bytes::Buf;
use futures::{Async, Future, Poll};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

use transport::{connect, Peek};

use super::{NewSensor, Sensor, Eos};

/// Wraps a transport with telemetry.
#[derive(Debug)]
pub struct Io<T> {
    io: T,
    sensor: Sensor,
}

/// Builds client transports with telemetry.
#[derive(Clone, Debug)]
pub struct Connect<C> {
    underlying: C,
    new_sensor: NewSensor,
}

/// Adds telemetry to a pending client transport.
#[derive(Clone, Debug)]
pub struct Connecting<C: connect::Connect> {
    underlying: C::Future,
    new_sensor: Option<NewSensor>,
}

// === impl Io ===

impl<T: AsyncRead + AsyncWrite> Io<T> {
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
                    let eos = e.raw_os_error()
                        .map(|e| Eos::Error(e.into()))
                        .unwrap_or(Eos::Clean);
                    self.sensor.record_close(eos);
                }

                Err(e)
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite> io::Read for Io<T> {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
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
        self.sense_err(|io| io.shutdown())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        let bytes = try_ready!(self.sense_err(|io| io.write_buf(buf)));
        self.sensor.record_write(bytes);

        Ok(Async::Ready(bytes))
    }
}

impl<T: AsyncRead + AsyncWrite + Peek> Peek for Io<T> {
    fn poll_peek(&mut self) -> Poll<usize, io::Error> {
        self.sense_err(|io| io.poll_peek())
    }

    fn peeked(&self) -> &[u8] {
        self.io.peeked()
    }
}

// === impl Connect ===

impl<C> Connect<C>
where
    C: connect::Connect,
{
    /// Returns a `Connect` to `addr` and `handle`.
    pub(super) fn new(underlying: C, new_sensor: NewSensor) -> Self {
        Self { underlying, new_sensor }
    }
}

impl<C> connect::Connect for Connect<C>
where
    C: connect::Connect,
{
    type Connected = Io<C::Connected>;
    type Error = C::Error;
    type Future = Connecting<C>;

    fn connect(&self) -> Self::Future {
        Connecting {
            underlying: self.underlying.connect(),
            new_sensor: Some(self.new_sensor.clone()),
        }
    }
}

// === impl Connecting ===

impl<C> Future for Connecting<C>
where
    C: connect::Connect,
{
    type Item = Io<C::Connected>;
    type Error = C::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = try_ready!(self.underlying.poll());
        debug!("client connection open");

        let sensor = self.new_sensor.take()
            .expect("future must not be polled after ready")
            .new_sensor();
        let t = Io::new(io, sensor);
        Ok(t.into())
    }
}
