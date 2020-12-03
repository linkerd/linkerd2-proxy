use super::{AsyncRead, AsyncWrite, Io, PeerAddr, Poll, ReadBuf, Result, IoSlice};
use std::{pin::Pin, task::Context};

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
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf<'_>) -> Poll<()> {
        self.as_mut().0.as_mut().poll_read(cx, buf)
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

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[IoSlice<'_>]) -> Poll<usize> {
        self.as_mut().0.as_mut().poll_write_vectored(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl crate::internal::Sealed for BoxedIo {}

impl std::fmt::Debug for BoxedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxedIo").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    pub struct WriteBufDetector;

    impl PeerAddr for WriteBufDetector {
        fn peer_addr(&self) -> Result<std::net::SocketAddr> {
            Ok(([0, 0, 0, 0], 0).into())
        }
    }

    impl AsyncRead for WriteBufDetector {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<()> {
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

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[std::io::IoSlice<'_>],
        ) -> Poll<usize> {
            Poll::Ready(Ok(0))
        }
    }


    impl crate::internal::Sealed for WriteBufDetector {}

    #[tokio::test]
    async fn boxed_io_uses_vectored_io() {
        use bytes::Bytes;
        let mut io = BoxedIo::new(WriteBufDetector);

        futures::future::poll_fn(|cx| {
            // This method will trigger the panic in WriteBufDetector::write IFF
            // BoxedIo doesn't call write_buf_erased, but write_buf, and triggering
            // a regular write.
            match Pin::new(&mut io).poll_write_buf_erased(cx, &mut Bytes::from("hello")) {
                Poll::Ready(Ok(_)) => {}
                _ => panic!("poll_write_buf should return Poll::Ready(Ok(_))"),
            }
            Poll::Ready(Ok(()))
        })
        .await
        .unwrap()
    }
}
