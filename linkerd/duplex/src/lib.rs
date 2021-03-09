#![deny(warnings, rust_2018_idioms)]

use bytes::{Buf, BufMut};
use futures::ready;
use linkerd_io::{self as io, AsyncRead, AsyncWrite};
use pin_project::pin_project;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tracing::trace;

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
    type Output = Result<(), io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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
        }
    }

    fn copy_into<U: AsyncWrite + Unpin>(
        &mut self,
        dst: &mut HalfDuplex<U>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // Since Duplex::poll() intentionally ignores the Async part of our
        // return value, we may be polled again after returning Ready, if the
        // other half isn't ready. In that case, if the destination has
        // shutdown, we finished in a previous poll, so don't even enter into
        // the copy loop.
        if dst.is_shutdown {
            trace!(direction = %self.direction, "already shutdown");
            return Poll::Ready(Ok(()));
        }

        // Proxy data until there's no more data to read.
        let mut wsz = 0;
        loop {
            if let Poll::Pending = self.poll_read(cx)? {
                break;
            }
            match self.poll_write_into(dst, cx)? {
                Poll::Pending => break,
                Poll::Ready(sz) => {
                    wsz += sz;
                }
            }

            if self.buf.is_none() {
                trace!(direction = %self.direction, "shutting down");
                debug_assert!(!dst.is_shutdown, "attempted to shut down destination twice");
                ready!(Pin::new(&mut dst.io).poll_shutdown(cx))?;
                dst.is_shutdown = true;
                return Poll::Ready(Ok(()));
            }
        }

        // If we attempted to write data, ensure it's flushed.
        if wsz > 0 {
            ready!(Pin::new(&mut dst.io).poll_flush(cx))?;
        }
        Poll::Pending
    }

    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let mut is_eof = false;
        let mut sz = 0;

        if let Some(ref mut buf) = self.buf {
            // If we've written all buffered data, read more.
            //
            // XXX should we read more data as long as there's buffer capacity?
            if !buf.has_remaining() {
                buf.reset();

                trace!(direction = %self.direction, "reading");
                let n = ready!(io::poll_read_buf(Pin::new(&mut self.io), cx, buf))?;
                trace!(direction = %self.direction, "read {}B", n);

                sz += n;
                is_eof = n == 0;
            }
        }

        if is_eof {
            trace!("eof");
            self.buf = None;
        }

        Poll::Ready(Ok(sz))
    }

    fn poll_write_into<U: AsyncWrite + Unpin>(
        &mut self,
        dst: &mut HalfDuplex<U>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<usize>> {
        let mut sz = 0;
        if let Some(ref mut buf) = self.buf {
            while buf.has_remaining() {
                trace!(direction = %self.direction, "writing {}B", buf.remaining());
                let n = ready!(io::poll_write_buf(Pin::new(&mut dst.io), cx, buf))?;
                trace!(direction = %self.direction, "wrote {}B", n);
                if n == 0 {
                    return Poll::Ready(Err(write_zero()));
                }
                sz += n;
            }
        }

        Poll::Ready(Ok(sz))
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
            buf: Box::new([0; 64 * 1024]),
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

unsafe impl BufMut for CopyBuf {
    fn remaining_mut(&self) -> usize {
        self.buf.len() - self.write_pos
    }

    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        unsafe {
            // this is, in fact, _totally fine and safe_: all the memory is
            // initialized.
            // there's just no way to turn a `&[T]` into a `&[MaybeUninit<T>]`
            // without ptr casting.
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

// #[cfg(test)]
// mod tests {
//     use std::io::{Error, Read, Result, Write};
//     use std::sync::atomic::{AtomicBool, Ordering};

//     use super::*;
//     use tokio::io::{AsyncRead, AsyncWrite};

//     #[derive(Debug)]
//     struct DoneIo(AtomicBool);

//     impl<'a> Read for &'a DoneIo {
//         fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
//             if self.0.swap(false, Ordering::Relaxed) {
//                 Ok(buf.len())
//             } else {
//                 Ok(0)
//             }
//         }
//     }

//     impl<'a> AsyncRead for &'a DoneIo {
//         unsafe fn prepare_uninitialized_buffer(&self, _buf: &mut [u8]) -> bool {
//             true
//         }
//     }

//     impl<'a> Write for &'a DoneIo {
//         fn write(&mut self, buf: &[u8]) -> Result<usize> {
//             Ok(buf.len())
//         }
//         fn flush(&mut self) -> Result<()> {
//             Ok(())
//         }
//     }
//     impl<'a> AsyncWrite for &'a DoneIo {
//         fn shutdown(&mut self) -> Poll<(), Error> {
//             if self.0.swap(false, Ordering::Relaxed) {
//                 Ok(Async::NotReady)
//             } else {
//                 Ok(Async::Ready(()))
//             }
//         }
//     }

//     #[test]
//     fn duplex_doesnt_hang_when_one_half_finishes() {
//         // Test reproducing an infinite loop in Duplex that caused issue #519,
//         // where a Duplex would enter an infinite loop when one half finishes.
//         let io_1 = DoneIo(AtomicBool::new(true));
//         let io_2 = DoneIo(AtomicBool::new(true));
//         let mut duplex = Duplex::new(&io_1, &io_2);

//         assert_eq!(duplex.poll().unwrap(), Async::NotReady);
//         assert_eq!(duplex.poll().unwrap(), Async::Ready(()));
//     }
// }
