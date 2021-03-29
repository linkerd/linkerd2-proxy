use crate::{IoSlice, PeerAddr, Poll};
use futures::ready;
use linkerd_errno::Errno;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::Context;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, Result};

pub trait Sensor {
    fn record_read(&mut self, sz: usize);
    fn record_write(&mut self, sz: usize);
    fn record_close(&mut self, eos: Option<Errno>);
    fn record_error<T>(&mut self, op: Poll<T>) -> Poll<T>;
}

/// Wraps a transport with telemetry.
#[pin_project]
#[derive(Debug)]
pub struct SensorIo<T, S> {
    #[pin]
    io: T,

    sensor: S,
}

// === impl SensorIo ===

impl<T, S: Sensor> SensorIo<T, S> {
    pub fn new(io: T, sensor: S) -> Self {
        Self { io, sensor }
    }
}

impl<T: AsyncRead + AsyncWrite, S: Sensor> AsyncRead for SensorIo<T, S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<()> {
        let this = self.project();
        let prev_filled = buf.filled().len();
        ready!(this.sensor.record_error(this.io.poll_read(cx, buf)))?;
        this.sensor.record_read(buf.filled().len() - prev_filled);
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncRead + AsyncWrite, S: Sensor> AsyncWrite for SensorIo<T, S> {
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project();
        this.sensor.record_error(this.io.poll_shutdown(cx))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.project();
        this.sensor.record_error(this.io.poll_flush(cx))
    }

    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<usize> {
        let this = self.project();
        let bytes = ready!(this.sensor.record_error(this.io.poll_write(cx, buf)))?;
        this.sensor.record_write(bytes);
        Poll::Ready(Ok(bytes))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<usize> {
        let this = self.project();
        let bytes = ready!(this
            .sensor
            .record_error(this.io.poll_write_vectored(cx, bufs)))?;
        this.sensor.record_write(bytes);
        Poll::Ready(Ok(bytes))
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

impl<T: PeerAddr, S> PeerAddr for SensorIo<T, S> {
    fn peer_addr(&self) -> Result<std::net::SocketAddr> {
        self.io.peer_addr()
    }
}
