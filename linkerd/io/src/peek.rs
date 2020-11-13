use crate::{AsyncRead, AsyncWrite, PrefixedIo};
use bytes::BytesMut;
use pin_project::pin_project;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::trace;

/// A future of when some `Peek` fulfills with some bytes.
#[derive(Debug)]
pub struct Peek<T>(Option<Inner<T>>);

#[pin_project]
#[derive(Debug)]
struct Inner<T> {
    buf: BytesMut,
    minimum: usize,

    #[pin]
    io: T,
}

pub trait Peekable: AsyncRead + AsyncWrite + Unpin {
    fn peek(self, minimum: usize, capacity: usize) -> Peek<Self>
    where
        Self: Sized,
    {
        Peek::with_capacity(minimum, capacity, self)
    }
}

impl<I: AsyncRead + AsyncWrite + Unpin> Peekable for I {}

// === impl Peek ===

impl<T: AsyncRead + AsyncWrite> Peek<T> {
    pub fn with_capacity(minimum: usize, capacity: usize, io: T) -> Self
    where
        Self: Sized + Future,
    {
        let buf = BytesMut::with_capacity(capacity);
        Peek(Some(Inner { buf, io, minimum }))
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
        let Inner { buf, io, .. } = this.0.take().expect("polled after complete");
        Poll::Ready(Ok(PrefixedIo::new(buf.freeze(), io)))
    }
}

// === impl Inner ===

impl<T: AsyncRead + Unpin> Inner<T> {
    fn poll_peek(&mut self, cx: &mut Context<'_>) -> Poll<Result<usize, io::Error>> {
        loop {
            match futures::ready!(Pin::new(&mut self.io).poll_read_buf(cx, &mut self.buf)) {
                Ok(sz) => {
                    // Return the number of buffered bytes if the inner stream
                    // has completed, the buffer is full, or the minimum buffer
                    // size has been met.
                    if sz == 0 || self.buf.len() >= self.minimum {
                        trace!(buf=%self.buf.len(), "Complete");
                        return Poll::Ready(Ok(self.buf.len()));
                    }
                    // Otherwise, continue reading.
                    trace!(sz, "Read");
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Peekable;

    #[tokio::test]
    async fn peek_minimum() {
        let io = tokio_test::io::Builder::new()
            .read(b"foo")
            .read(b"barbazbah")
            .build()
            .peek(6, 9)
            .await
            .unwrap();
        assert_eq!(io.prefix().as_ref(), b"foobarbaz");
    }

    #[tokio::test]
    async fn peek_eof() {
        let io = tokio_test::io::Builder::new()
            .read(b"foo")
            .build()
            .peek(6, 6)
            .await
            .unwrap();
        assert_eq!(io.prefix().as_ref(), b"foo");
    }
}
