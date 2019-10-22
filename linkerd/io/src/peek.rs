use crate::{AsyncRead, AsyncWrite, PrefixedIo};
use bytes::BytesMut;
use futures::{try_ready, Future, Poll};
use std::io;

/// A future of when some `Peek` fulfills with some bytes.
#[derive(Debug)]
pub struct Peek<T>(Option<Inner<T>>);

#[derive(Debug)]
struct Inner<T> {
    buf: BytesMut,
    io: T,
}

// === impl Peek ===

impl<T: AsyncRead + AsyncWrite> Peek<T> {
    pub fn with_capacity(capacity: usize, io: T) -> Self
    where
        Self: Sized + Future,
    {
        let buf = BytesMut::with_capacity(capacity);
        Peek(Some(Inner { buf, io }))
    }
}

impl<T: AsyncRead + AsyncWrite> Future for Peek<T> {
    type Item = PrefixedIo<T>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(self.0.as_mut().expect("polled after complete").poll_peek());
        let Inner { buf, io } = self.0.take().expect("polled after complete");
        Ok(PrefixedIo::new(buf.freeze(), io).into())
    }
}

// === impl Inner ===

impl<T: AsyncRead> Inner<T> {
    fn poll_peek(&mut self) -> Poll<usize, io::Error> {
        if self.buf.capacity() == 0 {
            return Ok(self.buf.len().into());
        }
        self.io.read_buf(&mut self.buf)
    }
}
