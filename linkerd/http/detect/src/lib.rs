use bytes::BytesMut;
use linkerd_error::{Error, Result};
use linkerd_http_variant::Variant;
use linkerd_io::{self as io, AsyncReadExt};
use linkerd_stack::{self as svc, ServiceExt};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;
use tracing::{debug, trace};

mod metrics;

pub use self::metrics::{DetectMetrics, DetectMetricsFamilies};

#[derive(Clone, Debug, Default)]
pub struct DetectParams {
    pub read_timeout: time::Duration,
    pub metrics: metrics::DetectMetrics,
}

#[derive(Debug, Clone)]
pub enum Detection {
    NotHttp,
    Http(Variant),
    ReadTimeout(time::Duration),
}

/// Attempts to detect the HTTP version of a stream.
///
/// This module biases towards availability instead of correctness. I.e. instead
/// of buffering until we can be sure that we're dealing with an HTTP stream, we
/// instead perform only a single read and use that data to inform protocol
/// hinting. If a single read doesn't provide enough data to make a decision, we
/// treat the protocol as unknown.
///
/// This allows us to interoperate with protocols that send very small initial
/// messages. In rare situations, we may fail to properly detect that a stream is
/// HTTP.
#[derive(Clone, Debug)]
pub struct Detect<N> {
    params: DetectParams,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct NewDetect<P, N> {
    inner: N,
    params: P,
}

#[derive(Debug, thiserror::Error)]
#[error("read timed out after {0:?}")]
pub struct ReadTimeoutError(pub time::Duration);

// Coincidentally, both our abbreviated H2 preface and our smallest possible
// HTTP/1 message are 14 bytes.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0";
const SMALLEST_POSSIBLE_HTTP1_REQ: &str = "GET / HTTP/1.1";

const READ_CAPACITY: usize = 1024;

// === impl NewDetect ===

impl<P, N> NewDetect<P, N> {
    pub fn new(params: P, inner: N) -> Self {
        Self { inner, params }
    }

    pub fn layer(params: P) -> impl svc::layer::Layer<N, Service = Self> + Clone
    where
        P: Clone,
    {
        svc::layer::mk(move |inner| Self::new(params.clone(), inner))
    }
}

impl<T, P, N> svc::NewService<T> for NewDetect<P, N>
where
    P: svc::ExtractParam<DetectParams, T>,
    N: svc::NewService<T>,
{
    type Service = Detect<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = self.params.extract_param(&target);
        Detect {
            params,
            inner: self.inner.new_service(target),
        }
    }
}

// === impl Detect ===

impl<I, N, NSvc> svc::Service<I> for Detect<N>
where
    I: io::AsyncRead + Send + Unpin + 'static,
    N: svc::NewService<Detection, Service = NSvc> + Clone + Send + 'static,
    NSvc: svc::Service<io::PrefixedIo<I>, Response = ()> + Send,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut io: I) -> Self::Future {
        let params = self.params.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let t0 = time::Instant::now();
            let mut buf = BytesMut::with_capacity(READ_CAPACITY);
            let result = detect(&params, &mut io, &mut buf).await;
            let elapsed = time::Instant::now().saturating_duration_since(t0);
            params.metrics.observe(&result, elapsed);
            debug!(?result, ?elapsed, "Detected");

            let detection = result?;
            trace!("Dispatching connection");
            let svc = inner.new_service(detection);
            let mut svc = svc.ready_oneshot().await.map_err(Into::into)?;
            svc.call(io::PrefixedIo::new(buf.freeze(), io))
                .await
                .map_err(Into::into)?;

            trace!("Connection completed");
            // Hold the service until it's done being used so that cache
            // idleness is reset.
            drop(svc);

            Ok(())
        })
    }
}

impl Detection {
    pub fn variant(&self) -> Option<Variant> {
        match self {
            Detection::Http(v) => Some(*v),
            _ => None,
        }
    }
}

async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
    params: &DetectParams,
    io: &mut I,
    buf: &mut BytesMut,
) -> io::Result<Detection> {
    debug_assert!(buf.capacity() > 0, "buffer must have capacity");

    trace!(capacity = buf.capacity(), timeout = ?params.read_timeout, "Reading");
    let sz = match time::timeout(params.read_timeout, io.read_buf(buf)).await {
        Ok(res) => res?,
        Err(_) => return Ok(Detection::ReadTimeout(params.read_timeout)),
    };

    trace!(sz, "Read");
    if sz == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "socket closed before protocol detection",
        ));
    }

    // HTTP/2 checking is faster because it's a simple string match. If we
    // have enough data, check it first. We don't bother matching on the
    // entire H2 preface because the first part is enough to get a clear
    // signal.
    if buf.len() >= H2_PREFACE.len() {
        trace!("Checking H2 preface");
        if &buf[..H2_PREFACE.len()] == H2_PREFACE {
            return Ok(Detection::Http(Variant::H2));
        }
    }

    // Otherwise, we try to parse the data as an HTTP/1 message.
    if buf.len() >= SMALLEST_POSSIBLE_HTTP1_REQ.len() {
        trace!("Parsing HTTP/1 message");
        if let Ok(_) | Err(httparse::Error::TooManyHeaders) =
            httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0]).parse(&buf[..])
        {
            return Ok(Detection::Http(Variant::Http1));
        }
    }

    Ok(Detection::NotHttp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io;

    const HTTP11_LINE: &[u8] = b"GET / HTTP/1.1\r\n";
    const H2_AND_GARBAGE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\ngarbage";
    const GARBAGE: &[u8] =
        b"garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage";

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout() {
        let _trace = linkerd_tracing::test::trace_init();

        let params = DetectParams {
            read_timeout: time::Duration::from_millis(1),
            ..Default::default()
        };
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().wait(params.read_timeout * 2).build();
        let kind = detect(&params, &mut io, &mut buf).await.unwrap();
        assert!(matches!(kind, Detection::ReadTimeout(_)), "{kind:?}");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn h2() {
        let _trace = linkerd_tracing::test::trace_init();

        let params = DetectParams {
            read_timeout: time::Duration::from_millis(1),
            ..Default::default()
        };
        for read in &[H2_PREFACE, H2_AND_GARBAGE] {
            debug!(read = ?std::str::from_utf8(read).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(read).build();
            let kind = detect(&params, &mut io, &mut buf).await.unwrap();
            assert_eq!(kind.variant(), Some(Variant::H2), "{kind:?}");
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn http1() {
        let _trace = linkerd_tracing::test::trace_init();

        let params = DetectParams {
            read_timeout: time::Duration::from_millis(1),
            ..Default::default()
        };
        for i in 1..SMALLEST_POSSIBLE_HTTP1_REQ.len() {
            debug!(read = ?std::str::from_utf8(&HTTP11_LINE[..i]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&HTTP11_LINE[..i]).build();
            let kind = detect(&params, &mut io, &mut buf).await.unwrap();
            assert!(matches!(kind, Detection::NotHttp), "{kind:?}");
        }

        debug!(read = ?std::str::from_utf8(HTTP11_LINE).unwrap());
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(HTTP11_LINE).build();
        let kind = detect(&params, &mut io, &mut buf).await.unwrap();
        assert_eq!(kind.variant(), Some(Variant::Http1), "{kind:?}");

        const REQ: &[u8] = b"GET /foo/bar/bar/blah HTTP/1.1\r\nHost: foob.example.com\r\n\r\n";
        for i in SMALLEST_POSSIBLE_HTTP1_REQ.len()..REQ.len() {
            debug!(read = ?std::str::from_utf8(&REQ[..i]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&REQ[..i]).build();
            let kind = detect(&params, &mut io, &mut buf).await.unwrap();
            assert_eq!(kind.variant(), Some(Variant::Http1), "{kind:?}");
            assert_eq!(buf[..], REQ[..i]);
        }

        // Starts with a P, like the h2 preface.
        const POST: &[u8] = b"POST /foo HTTP/1.1\r\n";
        for i in SMALLEST_POSSIBLE_HTTP1_REQ.len()..POST.len() {
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&POST[..i]).build();
            debug!(read = ?std::str::from_utf8(&POST[..i]).unwrap());
            let kind = detect(&params, &mut io, &mut buf).await.unwrap();
            assert_eq!(kind.variant(), Some(Variant::Http1), "{kind:?}");
            assert_eq!(buf[..], POST[..i]);
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn unknown() {
        let _trace = linkerd_tracing::test::trace_init();

        let params = DetectParams {
            read_timeout: time::Duration::from_millis(1),
            ..Default::default()
        };
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(b"foo.bar.blah\r\nbobo").build();
        let kind = detect(&params, &mut io, &mut buf).await.unwrap();
        assert!(matches!(kind, Detection::NotHttp), "{kind:?}");
        assert_eq!(&buf[..], b"foo.bar.blah\r\nbobo");

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(GARBAGE).build();
        let kind = detect(&params, &mut io, &mut buf).await.unwrap();
        assert!(matches!(kind, Detection::NotHttp), "{kind:?}");
        assert_eq!(&buf[..], GARBAGE);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn empty() {
        let _trace = linkerd_tracing::test::trace_init();

        let params = DetectParams {
            read_timeout: time::Duration::from_millis(1),
            ..Default::default()
        };
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().build();
        let err = detect(&params, &mut io, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "{err:?}");
        assert_eq!(&buf[..], b"");
    }
}
