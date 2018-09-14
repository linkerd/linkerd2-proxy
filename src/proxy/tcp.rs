use bytes::{Buf, BufMut};
use futures::{Async, Future, Poll};
use futures::future::{self, Either};
use std::{fmt, io};
use tokio::io::{AsyncRead, AsyncWrite};

use svc;
use transport::connect::Connect;

/// Attempt to proxy the `server_io` stream to a `T`-typed target.
///
/// If the trget is not valid, an error is logged and the server stream is
/// dropped.
pub(super) fn forward<I, C, T>(
    server_io: I,
    connect: &C,
    target: &T,
) -> impl Future<Item=(), Error=()> + Send + 'static
where
    T: fmt::Debug,
    I: AsyncRead + AsyncWrite + Send + 'static,
    C: svc::Stack<T>,
    C::Value: Connect,
    C::Error: fmt::Debug,
    <C::Value as Connect>::Future: Send + 'static,
    <C::Value as Connect>::Error: fmt::Debug,
    <C::Value as Connect>::Connected: Send + 'static,
{
    let connect = match connect.make(&target) {
        Ok(c) => c,
        Err(e) => {
            error!("forward error: {:?}: {:?}", target, e);
            return Either::A(future::ok(()));
        }
    };

    let fwd = connect.connect()
        .map_err(|e| info!("forward connect error: {:?}", e))
        .and_then(move |io| {
            Duplex::new(server_io, io)
                .map_err(|e| info!("forward duplex error: {}", e))
        });

    Either::B(fwd)
}

/// A future piping data bi-directionally to In and Out.
pub struct Duplex<In, Out> {
    half_in: HalfDuplex<In>,
    half_out: HalfDuplex<Out>,
}

struct HalfDuplex<T> {
    // None means socket met eof, and bytes have been drained into other half.
    buf: Option<CopyBuf>,
    is_shutdown: bool,
    io: T,
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
    In: AsyncRead + AsyncWrite,
    Out: AsyncRead + AsyncWrite,
{
    pub(super) fn new(in_io: In, out_io: Out) -> Self {
        Duplex {
            half_in: HalfDuplex::new(in_io),
            half_out: HalfDuplex::new(out_io),
        }
    }
}

impl<In, Out> Future for Duplex<In, Out>
where
    In: AsyncRead + AsyncWrite,
    Out: AsyncRead + AsyncWrite,
{
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // This purposefully ignores the Async part, since we don't want to
        // return early if the first half isn't ready, but the other half
        // could make progress.
        self.half_in.copy_into(&mut self.half_out)?;
        self.half_out.copy_into(&mut self.half_in)?;
        if self.half_in.is_done() && self.half_out.is_done() {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl<T> HalfDuplex<T>
where
    T: AsyncRead,
{
    fn new(io: T) -> Self {
        Self {
            buf: Some(CopyBuf::new()),
            is_shutdown: false,
            io,
        }
    }

    fn copy_into<U>(&mut self, dst: &mut HalfDuplex<U>) -> Poll<(), io::Error>
    where
        U: AsyncWrite,
    {
        // Since Duplex::poll() intentionally ignores the Async part of our
        // return value, we may be polled again after returning Ready, if the
        // other half isn't ready. In that case, if the destination has
        // shutdown, we finished in a previous poll, so don't even enter into
        // the copy loop.
        if dst.is_shutdown {
            return Ok(Async::Ready(()));
        }
        loop {
            try_ready!(self.read());
            try_ready!(self.write_into(dst));
            if self.buf.is_none() {
                debug_assert!(!dst.is_shutdown,
                    "attempted to shut down destination twice");
                try_ready!(dst.io.shutdown());
                dst.is_shutdown = true;

                return Ok(Async::Ready(()));
            }
        }


    }

    fn read(&mut self) -> Poll<(), io::Error> {
        let mut is_eof = false;
        if let Some(ref mut buf) = self.buf {
            if !buf.has_remaining() {
                buf.reset();
                let n = try_ready!(self.io.read_buf(buf));
                is_eof = n == 0;
            }
        }

        if is_eof {
            self.buf.take();
        }

        Ok(Async::Ready(()))
    }

    fn write_into<U>(&mut self, dst: &mut HalfDuplex<U>) -> Poll<(), io::Error>
    where
        U: AsyncWrite,
    {
        if let Some(ref mut buf) = self.buf {
            while buf.has_remaining() {
                let n = try_ready!(dst.io.write_buf(buf));
                if n == 0 {
                    return Err(write_zero());
                }
            }
        }

        Ok(Async::Ready(()))
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
            buf: Box::new([0; 4096]),
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

    fn bytes(&self) -> &[u8] {
        &self.buf[self.read_pos..self.write_pos]
    }

    fn advance(&mut self, cnt: usize) {
        assert!(self.write_pos >= self.read_pos + cnt);
        self.read_pos += cnt;
    }
}

impl BufMut for CopyBuf {
    fn remaining_mut(&self) -> usize {
        self.buf.len() - self.write_pos
    }

    unsafe fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.write_pos..]
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        assert!(self.buf.len() >= self.write_pos + cnt);
        self.write_pos += cnt;
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error, Read, Write, Result};
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncRead, AsyncWrite};
    use futures::{Async, Poll};
    use super::*;

    struct DoneIo(AtomicBool);

    impl<'a> Read for &'a DoneIo {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            if self.0.swap(false, Ordering::Relaxed) {
                Ok(buf.len())
            } else {
                Ok(0)
            }
        }
    }

    impl<'a> AsyncRead for &'a DoneIo {
        unsafe fn prepare_uninitialized_buffer(&self, _buf: &mut [u8]) -> bool {
            true
        }
    }

    impl<'a> Write for &'a DoneIo {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }
    impl<'a> AsyncWrite for &'a DoneIo {
        fn shutdown(&mut self) -> Poll<(), Error> {
            if self.0.swap(false, Ordering::Relaxed) {
                Ok(Async::NotReady)
            } else {
                Ok(Async::Ready(()))
            }
        }
    }

    #[test]
    fn duplex_doesnt_hang_when_one_half_finishes() {
        // Test reproducing an infinite loop in Duplex that caused issue #519,
        // where a Duplex would enter an infinite loop when one half finishes.
        let io_1 = DoneIo(AtomicBool::new(true));
        let io_2 = DoneIo(AtomicBool::new(true));
        let mut duplex = Duplex::new(&io_1, &io_2);

        assert_eq!(duplex.poll().unwrap(), Async::NotReady);
        assert_eq!(duplex.poll().unwrap(), Async::Ready(()));
    }

}
