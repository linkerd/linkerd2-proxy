mod client_hello;

use crate::ServerName;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_error::Error;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PrefixedIo};
use linkerd_stack::{layer, NewService, Service, ServiceExt};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time::{self, Duration};
use tracing::{debug, trace, warn};

pub type DetectIo<T> = EitherIo<T, PrefixedIo<T>>;

#[derive(Clone, Debug)]
pub struct NewDetectSNI<N> {
    inner: N,
    timeout: Timeout,
}

#[derive(Copy, Clone, Debug)]
pub struct Timeout(pub Duration);

#[derive(Clone, Debug, Error)]
#[error("SNI detection timed out")]
pub struct DetectSniTimeoutError(());

/// Attempts to detect an SNI from the client hello of a TLS session
#[derive(Clone, Debug)]
pub struct DetectSNI<T, N> {
    target: T,
    inner: N,
    timeout: Timeout,
}

// The initial peek buffer is fairly small so that we can avoid allocating more
// data then we need; but it is large enough to hold the ~300B ClientHello sent
// by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek
// buffer is insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<N> NewDetectSNI<N> {
    pub fn new(inner: N, timeout: Timeout) -> Self {
        Self { inner, timeout }
    }

    pub fn layer(timeout: Timeout) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(inner, timeout))
    }
}

impl<N, T> NewService<T> for NewDetectSNI<N>
where
    N: Clone,
{
    type Service = DetectSNI<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        DetectSNI {
            target,
            inner: self.inner.clone(),
            timeout: self.timeout,
        }
    }
}

impl<T, I, N, S> Service<I> for DetectSNI<T, N>
where
    T: Clone + Send + 'static,
    I: io::AsyncRead + io::Peek + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: NewService<(T, Option<ServerName>), Service = S> + Clone + Send + 'static,
    S: Service<DetectIo<I>> + Send,
    S::Error: Into<Error>,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let new_accept = self.inner.clone();

        // Detect the SNI from a ClientHello (or timeout).
        let Timeout(timeout) = self.timeout;
        let detect = time::timeout(timeout, detect_sni(io));
        Box::pin(async move {
            let (sni, io) = detect.await.map_err(|_| DetectSniTimeoutError(()))??;

            println!("detected SNI: {:?}", sni);
            let svc = new_accept.new_service((target, sni));
            svc.oneshot(io).await.map_err(Into::into)
        })
    }
}

/// Peek or buffer the provided stream to determine an SNI value.
async fn detect_sni<I>(mut io: I) -> io::Result<(Option<ServerName>, DetectIo<I>)>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin,
{
    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello. We
    // use a heap-allocated buffer to avoid creating a large `Future` (since we
    // need to hold the buffer across an await).
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a ~500B
    // byte buffer is more than enough.
    let mut buf = BytesMut::with_capacity(PEEK_CAPACITY);
    let sz = io.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    // Peek may return 0 bytes if the socket is not peekable.
    if sz > 0 {
        match client_hello::parse_sni(buf.as_ref()) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Left(io)));
            }

            Err(client_hello::Incomplete) => {}
        }
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while io.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match client_hello::parse_sni(buf.as_ref()) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Right(PrefixedIo::new(buf.freeze(), io))));
            }

            Err(client_hello::Incomplete) => {
                if buf.capacity() == 0 {
                    // If we can't buffer an entire TLS ClientHello, it
                    // almost definitely wasn't initiated by another proxy,
                    // at least.
                    warn!("Buffer insufficient for TLS ClientHello");
                    break;
                }
                // Continue if there is still buffer capacity.
            }
        }
    }

    trace!("Could not read TLS ClientHello via buffering");
    let io = EitherIo::Right(PrefixedIo::new(buf.freeze(), io));
    Ok((None, io))
}

impl From<Duration> for Timeout {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_io::AsyncWriteExt;

    #[tokio::test(flavor = "current_thread")]
    async fn detect_buffered() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut client_io, server_io) = linkerd_io::duplex(1024);
        let input = include_bytes!("detect_sni/testdata/curl-example-com-client-hello.bin");
        let len = input.len();
        let client_task = tokio::spawn(async move {
            client_io
                .write_all(input)
                .await
                .expect("Write must succeed");
        });

        let (sni, io) = detect_sni(server_io)
            .await
            .expect("SNI detection must not fail");

        assert_eq!(sni, Some(ServerName("example.com".parse().unwrap())));

        match io {
            EitherIo::Left(_) => panic!("Detected IO should be buffered"),
            EitherIo::Right(io) => assert_eq!(io.prefix().len(), len, "All data must be buffered"),
        }

        client_task.await.expect("Client must not fail");
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;

    pub fn fuzz_entry(input: &[u8]) {
        let _ = client_hello::parse_sni(input);
    }
}
