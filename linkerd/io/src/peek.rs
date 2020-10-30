use crate::{AsyncRead, AsyncWrite, PrefixedIo};
use bytes::BytesMut;
use pin_project::pin_project;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A future of when some `Peek` fulfills with some bytes.
#[derive(Debug)]
pub struct Peek<T>(Option<Inner<T>>);

#[pin_project]
#[derive(Debug)]
struct Inner<T> {
    buf: BytesMut,

    #[pin]
    io: T,
}

pub trait Peekable: AsyncRead + AsyncWrite + Unpin {
    fn peek(self, capacity: usize) -> Peek<Self>
    where
        Self: Sized,
    {
        Peek::with_capacity(capacity, self)
    }
}

impl<I: AsyncRead + AsyncWrite + Unpin> Peekable for I {}

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

impl<T: AsyncRead + AsyncWrite + Unpin> Future for Peek<T> {
    type Output = Result<PrefixedIo<T>, io::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();
        futures::ready!(this
            .0
            .as_mut()
            .expect("polled after complete")
            .poll_peek(cx))?;
        let Inner { buf, io } = this.0.take().expect("polled after complete");
        Poll::Ready(Ok(PrefixedIo::new(buf.freeze(), io)))
    }
}

// === impl Inner ===

impl<T: AsyncRead + AsyncWrite + Unpin> Inner<T> {
    fn poll_peek(&mut self, cx: &mut Context<'_>) -> Poll<Result<usize, io::Error>> {
        if self.buf.capacity() == 0 {
            return Poll::Ready(Ok(self.buf.len()));
        }
        crate::poll_read_buf(cx, Pin::new(&mut self.io), &mut self.buf)
    }
}
