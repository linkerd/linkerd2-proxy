use super::Sensor;
use bytes::{Buf, BufMut};
use futures::ready;
use pin_project::pin_project;
use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};

/// Wraps a transport with telemetry.
#[pin_project]
#[derive(Debug)]
pub struct Io<T> {
    #[pin]
    io: T,

    sensor: Sensor,
}

// === impl Io ===

impl<T> Io<T> {
    pub(super) fn new(io: T, sensor: Sensor) -> Self {
        Self { io, sensor }
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncRead for Io<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let bytes = ready!(this.sensor.sense_err(this.io.poll_read(cx, buf)))?;
        this.sensor.record_read(bytes);
        Poll::Ready(Ok(bytes))
    }

    fn poll_read_buf<B: BufMut>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let bytes = ready!(this.sensor.sense_err(this.io.poll_read_buf(cx, buf)))?;
        this.sensor.record_read(bytes);
        Poll::Ready(Ok(bytes))
    }

    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncWrite for Io<T> {
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.sensor.sense_err(this.io.poll_shutdown(cx))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.sensor.sense_err(this.io.poll_flush(cx))
    }

    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let bytes = ready!(this.sensor.sense_err(this.io.poll_write(cx, buf)))?;
        this.sensor.record_write(bytes);
        Poll::Ready(Ok(bytes))
    }

    fn poll_write_buf<B: Buf>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let bytes = ready!(this.sensor.sense_err(this.io.poll_write_buf(cx, buf)))?;
        this.sensor.record_write(bytes);
        Poll::Ready(Ok(bytes))
    }
}
