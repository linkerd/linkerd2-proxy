use super::{AsyncRead, AsyncWrite, Io, PeerAddr, Poll, ReadBuf};
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
    fn peer_addr(&self) -> std::net::SocketAddr {
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
}

impl crate::internal::Sealed for BoxedIo {}

impl std::fmt::Debug for BoxedIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxedIo").finish()
    }
}

// TODO(eliza): Tokio 0.3 doesn't have writev support. Bring all this back when
// vectored writes work again.
// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[derive(Debug)]
//     struct WriteBufDetector;

//     impl PeerAddr for WriteBufDetector {
//         fn peer_addr(&self) -> std::net::SocketAddr {
//             ([0, 0, 0, 0], 0).into()
//         }
//     }

//     impl AsyncRead for WriteBufDetector {
//         fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut [u8]) -> Poll<usize> {
//             unreachable!("not called in test")
//         }
//     }

//     impl AsyncWrite for WriteBufDetector {
//         fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
//             unreachable!("not called in test")
//         }

//         fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
//             unreachable!("not called in test")
//         }

//         fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<usize> {
//             panic!("BoxedIo called wrong write_buf method");
//         }

//         fn poll_write_buf<B: bytes::Buf>(
//             self: Pin<&mut Self>,
//             _: &mut Context<'_>,
//             _: &mut B,
//         ) -> Poll<usize> {
//             Poll::Ready(Ok(0))
//         }
//     }

//     impl Io for WriteBufDetector {
//         fn poll_write_buf_erased(
//             self: Pin<&mut Self>,
//             cx: &mut Context<'_>,
//             mut buf: &mut dyn Buf,
//         ) -> Poll<usize> {
//             self.poll_write_buf(cx, &mut buf)
//         }

//         fn poll_read_buf_erased(
//             self: Pin<&mut Self>,
//             _: &mut Context<'_>,
//             _: &mut dyn BufMut,
//         ) -> Poll<usize> {
//             unreachable!("not called in test")
//         }
//     }

//     #[tokio::test]
//     async fn boxed_io_uses_vectored_io() {
//         use bytes::Bytes;
//         let mut io = BoxedIo::new(WriteBufDetector);

//         futures::future::poll_fn(|cx| {
//             // This method will trigger the panic in WriteBufDetector::write IFF
//             // BoxedIo doesn't call write_buf_erased, but write_buf, and triggering
//             // a regular write.
//             match Pin::new(&mut io).poll_write_buf(cx, &mut Bytes::from("hello")) {
//                 Poll::Ready(Ok(_)) => {}
//                 _ => panic!("poll_write_buf should return Poll::Ready(Ok(_))"),
//             }
//             Poll::Ready(Ok(()))
//         })
//         .await
//         .unwrap()
//     }
// }
