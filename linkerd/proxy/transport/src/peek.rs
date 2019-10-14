use futures::{Async, Future, Poll};
use std::io;

pub trait Peek {
    /// An async attempt to peek bytes of this type without consuming.
    ///
    /// Returns number of bytes that have been peeked.
    fn poll_peek(&mut self) -> Poll<usize, io::Error>;

    /// Returns a reference to the bytes that have been peeked.
    // Instead of passing a buffer into `peek()`, the bytes are kept in
    // a buffer owned by the `Peek` type. This allows looking at the
    // peeked bytes cheaply, instead of needing to copy into a new
    // buffer.
    fn peeked(&self) -> &[u8];

    /// A `Future` around `poll_peek`, returning this type instead.
    fn peek(self) -> PeekFuture<Self>
    where
        Self: Sized,
    {
        PeekFuture(Some(self))
    }
}

/// A future of when some `Peek` fulfills with some bytes.
#[derive(Debug)]
pub struct PeekFuture<T>(Option<T>);

// === impl PeekFuture ===

impl<T: Peek> Future for PeekFuture<T> {
    type Item = T;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut io = self.0.take().expect("polled after completed");
        match io.poll_peek() {
            Ok(Async::Ready(_)) => Ok(Async::Ready(io)),
            Ok(Async::NotReady) => {
                self.0 = Some(io);
                Ok(Async::NotReady)
            }
            Err(e) => Err(e),
        }
    }
}
