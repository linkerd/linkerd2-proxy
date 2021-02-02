use crate::Version;
use bytes::BytesMut;
use linkerd_detect::Detect;
use linkerd_error::Error;
use linkerd_io::{self as io, AsyncReadExt};
use tracing::{debug, trace};

// Coincidentally, both our abbreviated H2 preface and our smallest possible
// HTTP/1 message are 14 bytes.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0";
const SMALLEST_POSSIBLE_HTTP1_REQ: &str = "GET / HTTP/1.1";

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
#[derive(Clone, Debug, Default)]
pub struct DetectHttp(());

#[async_trait::async_trait]
impl Detect for DetectHttp {
    type Protocol = Version;

    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Option<Version>, Error> {
        trace!(capacity = buf.capacity(), "Reading");
        let sz = io.read_buf(buf).await?;
        trace!(sz, "Read");
        if sz == 0 {
            // No data was read because the socket closed or the
            // buffer capacity was exhausted.
            debug!(read = buf.len(), "Could not detect protocol");
            return Ok(None);
        }

        // HTTP/2 checking is faster because it's a simple string match. If we
        // have enough data, check it first. We don't bother matching on the
        // entire H2 preface because the first part is enough to get a clear
        // signal.
        if buf.len() >= H2_PREFACE.len() {
            trace!("Checking H2 preface");
            if &buf[..H2_PREFACE.len()] == H2_PREFACE {
                trace!("Matched HTTP/2 prefix");
                return Ok(Some(Version::H2));
            }
        }

        // Otherwise, we try to parse the data as an HTTP/1 message.
        if buf.len() >= SMALLEST_POSSIBLE_HTTP1_REQ.len() {
            trace!("Parsing HTTP/1 message");
            if let Ok(_) | Err(httparse::Error::TooManyHeaders) =
                httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0]).parse(&buf[..])
            {
                trace!("Matched HTTP/1");
                return Ok(Some(Version::Http1));
            }
        }

        trace!("Not HTTP");
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test::io;

    const HTTP11_LINE: &[u8] = b"GET / HTTP/1.1\r\n";
    const H2_AND_GARBAGE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\ngarbage";
    const GARBAGE: &[u8] =
        b"garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage";

    #[tokio::test]
    async fn h2() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        for read in &[H2_PREFACE, H2_AND_GARBAGE] {
            debug!(read = ?std::str::from_utf8(&read).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&read).build();
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, Some(Version::H2));
        }
    }

    #[tokio::test]
    async fn http1() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        // If we don't read enough to know
        for i in 1..SMALLEST_POSSIBLE_HTTP1_REQ.len() {
            debug!(read = ?std::str::from_utf8(&HTTP11_LINE[..i]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&HTTP11_LINE[..i]).build();
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, None);
        }

        debug!(read = ?std::str::from_utf8(&HTTP11_LINE).unwrap());
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(&HTTP11_LINE).build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, Some(Version::Http1));

        const REQ: &[u8] = b"GET /foo/bar/bar/blah HTTP/1.1\r\nHost: foob.example.com\r\n\r\n";
        for i in SMALLEST_POSSIBLE_HTTP1_REQ.len()..REQ.len() {
            debug!(read = ?std::str::from_utf8(&REQ[..i]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&REQ[..i]).build();
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, Some(Version::Http1));
            assert_eq!(buf[..], REQ[..i]);
        }

        // Starts with a P, like the h2 preface.
        const POST: &[u8] = b"POST /foo HTTP/1.1\r\n";
        for i in SMALLEST_POSSIBLE_HTTP1_REQ.len()..POST.len() {
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new().read(&POST[..i]).build();
            debug!(read = ?std::str::from_utf8(&POST[..i]).unwrap());
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, Some(Version::Http1));
            assert_eq!(buf[..], POST[..i]);
        }
    }

    #[tokio::test]
    async fn unknown() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(b"foo.bar.blah\r\nbobo").build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, None);
        assert_eq!(&buf[..], b"foo.bar.blah\r\nbobo");

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(GARBAGE).build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, None);
        assert_eq!(&buf[..], GARBAGE);
    }
}
