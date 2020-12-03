use super::{internal::Io, AsyncRead, AsyncWrite, PeerAddr, Poll, Result};
use bytes::{Buf, BufMut};
use std::{mem::MaybeUninit, pin::Pin, task::Context};

/// A public wrapper around a `Box<Io>`.
///
/// This type ensures that the proper write_buf method is called,
/// to allow vectored writes to occur.
pub struct BoxedIo(Pin<Box<dyn Io + Unpin>>);

impl BoxedIo {
    pub fn new<T: Io + Unpin + 'static>(io: T) -> Self {
        BoxedIo(Box::pin(io))
    }
}

impl PeerAddr for BoxedIo {
    fn peer_addr(&self) -> Result<std::net::SocketAddr> {
        self.0.peer_addr()
    }
}

impl AsyncRead for BoxedIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<usize> {
        self.as_mut().0.as_mut().poll_read(cx, buf)
    }

    fn poll_read_buf<B: BufMut>(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        mut buf: &mut B,
    ) -> Poll<usize> {
        // A trait object of AsyncWrite would use the default poll_read_buf,
        // which doesn't allow vectored reads. Going through this method
        // allows the trait object to call the specialized poll_read_buf method.
        self.as_mut().0.as_mut().poll_read_buf_erased(cx, &mut buf)
    }

    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
        self.0.prepare_uninitialized_buffer(buf)
    }
}

impl AsyncWrite for BoxedIo {
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        self.as_mut().0.as_mut().poll_shutdown(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        self.as_mut().0.as_mut().poll_flush(cx)
    }

    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<usize> {
        self.as_mut().0.as_mut().poll_write(cx, buf)
    }

    fn poll_write_buf<B: Buf>(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut B,
    ) -> Poll<usize>
    where
        Self: Sized,
    {
        // A trait object of AsyncWrite would use the default poll_write_buf,
        // which doesn't allow vectored writes. Going through this method
        // allows the trait object to call the specialized poll_write_buf
        // method.
        self.as_mut().0.as_mut().poll_write_buf_erased(cx, buf)
    }
}

impl Io for BoxedIo {
    /// This method is to allow using `Async::poll_write_buf` even through a
    /// trait object.
    fn poll_write_buf_erased(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut dyn Buf,
    ) -> Poll<usize> {
        self.as_mut().0.as_mut().poll_write_buf_erased(cx, buf)
    }

    /// This method is to allow using `Async::poll_read_buf` even through a
    /// trait object.
    fn poll_read_buf_erased(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut dyn BufMut,
    ) -> Poll<usize> {
        self.as_mut().0.as_mut().poll_read_buf_erased(cx, buf)
    }
}

impl std::fmt::Debug for BoxedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxedIo").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct WriteBufDetector;

    impl PeerAddr for WriteBufDetector {
        fn peer_addr(&self) -> Result<std::net::SocketAddr> {
            Ok(([0, 0, 0, 0], 0).into())
        }
    }

    impl AsyncRead for WriteBufDetector {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut [u8]) -> Poll<usize> {
            unreachable!("not called in test")
        }
    }

    impl AsyncWrite for WriteBufDetector {
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            unreachable!("not called in test")
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            unreachable!("not called in test")
        }

        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<usize> {
            panic!("BoxedIo called wrong write_buf method");
        }

        fn poll_write_buf<B: bytes::Buf>(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut B,
        ) -> Poll<usize> {
            Poll::Ready(Ok(0))
        }
    }

    impl Io for WriteBufDetector {
        fn poll_write_buf_erased(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            mut buf: &mut dyn Buf,
        ) -> Poll<usize> {
            self.poll_write_buf(cx, &mut buf)
        }

        fn poll_read_buf_erased(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut dyn BufMut,
        ) -> Poll<usize> {
            unreachable!("not called in test")
        }
    }

    #[tokio::test]
    async fn boxed_io_uses_vectored_io() {
        use bytes::Bytes;
        let mut io = BoxedIo::new(WriteBufDetector);

        futures::future::poll_fn(|cx| {
            // This method will trigger the panic in WriteBufDetector::write IFF
            // BoxedIo doesn't call write_buf_erased, but write_buf, and triggering
            // a regular write.
            match Pin::new(&mut io).poll_write_buf(cx, &mut Bytes::from("hello")) {
                Poll::Ready(Ok(_)) => {}
                _ => panic!("poll_write_buf should return Poll::Ready(Ok(_))"),
            }
            Poll::Ready(Ok(()))
        })
        .await
        .unwrap()
    }
}
