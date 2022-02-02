//! A utility for copying data bi-directionally between two sockets.
//!
//! This module uses unsafe code to implement [`BufMut`].

#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type,
    unsafe_code
)]

use bytes::{Buf, BufMut};
use futures::ready;
use linkerd_io::{self as io, AsyncRead, AsyncWrite};
use pin_project::pin_project;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tracing::{error, trace};

/// A future piping data bi-directionally to In and Out.
#[pin_project]
pub struct Duplex<In, Out> {
    half_in: HalfDuplex<In>,
    half_out: HalfDuplex<Out>,
}

#[pin_project]
struct HalfDuplex<T> {
    // None means socket met eof, and bytes have been drained into other half.
    buf: Option<CopyBuf>,
    is_shutdown: bool,
    #[pin]
    io: T,
    direction: &'static str,
    flushing: bool,
}

/// A buffer used to copy bytes from one IO to another.
///
/// Keeps read and write positions.
struct CopyBuf {
    // TODO:
    // In linkerd-tcp, a shared buffer is used to start, and an allocation is
    // only made if NotReady is found trying to flush the buffer. We could
    // consider making the same optimization here.
    buf: Box<[u8]>,
    read_pos: usize,
    write_pos: usize,
}

enum Buffered {
    NotEmpty,
    Read(usize),
    Eof,
}

enum Drained {
    BufferEmpty,
    Partial(usize),
    All(usize),
}

impl<In, Out> Duplex<In, Out>
where
    In: AsyncRead + AsyncWrite + Unpin,
    Out: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(in_io: In, out_io: Out) -> Self {
        Duplex {
            half_in: HalfDuplex::new(in_io, "client->server"),
            half_out: HalfDuplex::new(out_io, "server->client"),
        }
    }
}

impl<In, Out> Future for Duplex<In, Out>
where
    In: AsyncRead + AsyncWrite + Unpin,
    Out: AsyncRead + AsyncWrite + Unpin,
{
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Poll<()> {
        let mut this = self.project();
        // This purposefully ignores the Async part, since we don't want to
        // return early if the first half isn't ready, but the other half
        // could make progress.
        trace!("poll");
        let _ = this.half_in.copy_into(&mut this.half_out, cx)?;
        let _ = this.half_out.copy_into(&mut this.half_in, cx)?;
        if this.half_in.is_done() && this.half_out.is_done() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl<T> HalfDuplex<T>
where
    T: AsyncRead + Unpin,
{
    fn new(io: T, direction: &'static str) -> Self {
        Self {
            buf: Some(CopyBuf::new()),
            is_shutdown: false,
            io,
            direction,
            flushing: false,
        }
    }

    /// Reads data from `self`, buffering it, and writing it to `dst.
    ///
    /// Returns ready when the stream has shutdown such that no more data may be
    /// proxied.
    fn copy_into<U: AsyncWrite + Unpin>(
        &mut self,
        dst: &mut HalfDuplex<U>,
        cx: &mut Context<'_>,
    ) -> io::Poll<()> {
        // Since Duplex::poll() intentionally ignores the Async part of our
        // return value, we may be polled again after returning Ready, if the
        // other half isn't ready. In that case, if the destination has
        // shutdown, we finished in a previous poll, so don't even enter into
        // the copy loop.
        if dst.is_shutdown {
            trace!(direction = %self.direction, "already shutdown");
            return Poll::Ready(Ok(()));
        }

        // If the last invocation returned pending while flushing, resume
        // flushing and only proceed when the flush is complete.
        if self.flushing {
            ready!(self.poll_flush(dst, cx))?;
        }

        // `needs_flush` is set to true if the buffer is written so that, if a
        // read returns pending, that data may be flushed.
        let mut needs_flush = false;

        loop {
            // As long as the underlying socket is alive, ensure we've read data
            // from it into the local buffer.
            match self.poll_buffer(cx)? {
                Poll::Pending => {
                    // If there's no data to be read and we've written data, try
                    // flushing before returning pending.
                    if needs_flush {
                        // The poll status of the flush isn't relevant, as we
                        // have registered interest in the read (and maybe the
                        // write as well). If the flush did not complete
                        // `self.flushing` is true so that it may be resumed on
                        // the next poll.
                        let _ = self.poll_flush(dst, cx)?;
                    }
                    return Poll::Pending;
                }

                Poll::Ready(Buffered::NotEmpty) | Poll::Ready(Buffered::Read(_)) => {
                    // Write buffered data to the destination.
                    match self.drain_into(dst, cx)? {
                        // All of the buffered data was written, so continue reading more.
                        Drained::All(sz) => {
                            debug_assert!(sz > 0);
                            needs_flush = true;
                        }
                        // Only some of the buffered data could be written
                        // before the destination became pending. Try to flush
                        // the written data to get capacity.
                        Drained::Partial(_) => {
                            ready!(self.poll_flush(dst, cx))?;
                            // If the flush completed, try writing again to
                            // ensure that we have a notification registered. If
                            // all of the buffered data still cannot be written,
                            // return pending. Otherwise, continue.
                            if let Drained::Partial(_) = self.drain_into(dst, cx)? {
                                return Poll::Pending;
                            }
                            needs_flush = false;
                        }
                        Drained::BufferEmpty => {
                            error!(
                                direction = self.direction,
                                "Invalid state: attempted to write from an empty buffer"
                            );
                            debug_assert!(false, "The write buffer should never be empty");
                            return Poll::Ready(Ok(()));
                        }
                    }
                }

                // The socket closed, so initiate shutdown on the destination.
                Poll::Ready(Buffered::Eof) => {
                    trace!(direction = %self.direction, "shutting down");
                    debug_assert!(!dst.is_shutdown, "attempted to shut down destination twice");
                    ready!(Pin::new(&mut dst.io).poll_shutdown(cx))?;
                    dst.is_shutdown = true;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }

    /// Attempts to read and buffer data from the underlying stream, returning
    /// the number of bytes read. If the buffer already has data, no new data
    /// will be read.
    fn poll_buffer(&mut self, cx: &mut Context<'_>) -> io::Poll<Buffered> {
        // Buffer data only if no data is buffered.
        //
        // TODO should we read more data as long as there's buffer capacity?
        // To do this, we'd have to get more complex about handling EOF.
        if let Some(buf) = self.buf.as_mut() {
            if buf.has_remaining() {
                // Data was already buffered, so just return immediately.
                trace!(direction = %self.direction, remaining = buf.remaining(), "skipping read");
                return Poll::Ready(Ok(Buffered::NotEmpty));
            }

            buf.reset();
            trace!(direction = %self.direction, "reading");
            let sz = ready!(io::poll_read_buf(Pin::new(&mut self.io), cx, buf))?;
            trace!(direction = %self.direction, "read {}B", sz);

            // If data was read, return the number of bytes read.
            if sz > 0 {
                return Poll::Ready(Ok(Buffered::Read(sz)));
            }
        }

        // No more data can be read.
        trace!("eof");
        self.buf = None;
        Poll::Ready(Ok(Buffered::Eof))
    }

    /// Attempts to flush the destination. `self.flushing` is set to true iff the
    /// flush operation did not complete.
    fn poll_flush<U: AsyncWrite + Unpin>(
        &mut self,
        dst: &mut HalfDuplex<U>,
        cx: &mut Context<'_>,
    ) -> io::Poll<()> {
        trace!(direction = %self.direction, "flushing");
        let poll = Pin::new(&mut dst.io).poll_flush(cx);
        self.flushing = poll.is_pending();
        if poll.is_ready() {
            trace!(direction = %self.direction, "flushed");
        }
        poll
    }

    /// Writes as much buffered data as possible, returning the number of bytes written.
    fn drain_into<U: AsyncWrite + Unpin>(
        &mut self,
        dst: &mut HalfDuplex<U>,
        cx: &mut Context<'_>,
    ) -> io::Result<Drained> {
        let mut sz = 0;

        if let Some(buf) = self.buf.as_mut() {
            while buf.has_remaining() {
                trace!(direction = %self.direction, "writing {}B", buf.remaining());
                let n = match io::poll_write_buf(Pin::new(&mut dst.io), cx, buf)? {
                    Poll::Pending => return Ok(Drained::Partial(sz)),
                    Poll::Ready(n) => n,
                };
                trace!(direction = %self.direction, "wrote {}B", n);
                if n == 0 {
                    return Err(write_zero());
                }
                sz += n;
            }
        }

        if sz == 0 {
            Ok(Drained::BufferEmpty)
        } else {
            Ok(Drained::All(sz))
        }
    }

    fn is_done(&self) -> bool {
        self.is_shutdown
    }
}

fn write_zero() -> io::Error {
    io::Error::new(io::ErrorKind::WriteZero, "write zero bytes")
}

impl CopyBuf {
    fn new() -> Self {
        CopyBuf {
            buf: Box::new([0; 8 * 1024]),
            read_pos: 0,
            write_pos: 0,
        }
    }

    fn reset(&mut self) {
        debug_assert_eq!(self.read_pos, self.write_pos);
        self.read_pos = 0;
        self.write_pos = 0;
    }
}

impl Buf for CopyBuf {
    fn remaining(&self) -> usize {
        self.write_pos - self.read_pos
    }

    fn chunk(&self) -> &[u8] {
        &self.buf[self.read_pos..self.write_pos]
    }

    fn advance(&mut self, cnt: usize) {
        assert!(self.write_pos >= self.read_pos + cnt);
        self.read_pos += cnt;
    }
}

#[allow(unsafe_code)]
unsafe impl BufMut for CopyBuf {
    fn remaining_mut(&self) -> usize {
        self.buf.len() - self.write_pos
    }

    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        // Safety: The memory is initialized. This is the only way to turn a
        // `&[T]` into a `&[MaybeUninit<T>]` without ptr casting.
        unsafe {
            bytes::buf::UninitSlice::from_raw_parts_mut(
                &mut self.buf[self.write_pos] as *mut _,
                self.buf.len() - self.write_pos,
            )
        }
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        assert!(self.buf.len() >= self.write_pos + cnt);
        self.write_pos += cnt;
    }
}
