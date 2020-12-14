use crate::Version;
use bytes::BytesMut;
use linkerd2_error::Error;
use linkerd2_io::{self as io, AsyncReadExt};
use linkerd2_proxy_transport::{Detect, NewDetectService};
use linkerd2_stack::layer;
use tokio::time;
use tracing::{debug, trace};

const H2_PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[derive(Clone, Debug, Default)]
pub struct DetectHttp(());

impl DetectHttp {
    pub fn layer<N>(
        timeout: time::Duration,
    ) -> impl layer::Layer<N, Service = NewDetectService<N, Self>> + Clone {
        NewDetectService::layer(timeout, Self(()))
    }
}

#[async_trait::async_trait]
impl Detect for DetectHttp {
    type Protocol = Version;

    async fn detect<I: io::AsyncRead + Send + Unpin + 'static>(
        &self,
        io: &mut I,
        buf: &mut BytesMut,
    ) -> Result<Option<Version>, Error> {
        let mut scan_idx = 0;
        let mut maybe_h1 = true;
        let mut maybe_h2 = true;

        loop {
            // Read data from the socket or timeout detection.
            trace!(
                capacity = buf.capacity(),
                scan_idx,
                maybe_h1,
                maybe_h2,
                "Reading"
            );
            let sz = io.read_buf(buf).await?;
            if sz == 0 {
                // No data was read because the socket closed or the
                // buffer capacity was exhausted.
                debug!(read = buf.len(), "Could not detect protocol");
                return Ok(None);
            }

            // HTTP/2 checking is faster because it's a simple string match. If
            // we have enough data, check it first. In almost all cases, the
            // whole preface should be available from the first read.
            if maybe_h2 {
                if buf.len() < H2_PREFACE.len() {
                    // Check the prefix we have already read to see if it looks likely to be HTTP/2.
                    if buf[..] == H2_PREFACE[..buf.len()] {
                        // If it looks like HTTP/2, it's not HTTP/1.
                        maybe_h1 = false;
                    } else {
                        maybe_h2 = false;
                    }
                } else {
                    trace!("Checking H2 preface");
                    if &buf[..H2_PREFACE.len()] == H2_PREFACE {
                        trace!("Matched HTTP/2 prefix");
                        return Ok(Some(Version::H2));
                    }

                    // Not a match. Don't check for an HTTP/2 preface again.
                    maybe_h2 = false;
                }
            }

            if maybe_h1 {
                // Scan up to the first line ending to determine whether the
                // request is HTTP/1.1.
                for i in scan_idx..(buf.len() - 1) {
                    if &buf[i..=i + 1] == b"\r\n" {
                        trace!(offset = i, "Found newline");
                        // If the first line looks like an HTTP/2 first line,
                        // then we almost definitely got a fragmented first
                        // read. Only try HTTP/1 parsing if it doesn't look like
                        // HTTP/2.
                        if !maybe_h2 {
                            trace!("Parsing HTTP/1 message");
                            // If we get to reading headers (and fail), the
                            // first line looked like an HTTP/1 request; so
                            // handle the stream as HTTP/1.
                            if let Ok(_) | Err(httparse::Error::TooManyHeaders) =
                                httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0])
                                    .parse(&buf[..])
                            {
                                trace!("Matched HTTP/1");
                                return Ok(Some(Version::Http1));
                            }
                        }

                        // We found the EOL and it wasn't an HTTP/1.x request;
                        // stop scanning and don't scan again.
                        maybe_h1 = false;
                        break;
                    }
                }

                // Advance our scan index to the end of buffer so the next
                // iteration starts scanning where we left off.
                scan_idx = buf.len() - 1;
            }

            if !maybe_h1 && !maybe_h2 {
                trace!("Not HTTP");
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;
    use tokio_test::io;

    const HTTP11_LINE: &'static [u8] = b"GET / HTTP/1.1\r\n";
    const H2_AND_GARBAGE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\ngarbage";
    const GARBAGE: &'static [u8] =
        b"garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage garbage";

    #[tokio::test]
    async fn h2() {
        let _ = tracing_subscriber::fmt::try_init();

        for i in 1..H2_PREFACE.len() {
            let mut buf = BytesMut::with_capacity(H2_PREFACE.len());
            buf.put(H2_AND_GARBAGE);
            debug!(read0 = ?std::str::from_utf8(&H2_AND_GARBAGE[..i]).unwrap());
            debug!(read1 = ?std::str::from_utf8(&H2_AND_GARBAGE[i..]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new()
                .read(&H2_AND_GARBAGE[..i])
                .read(&H2_AND_GARBAGE[i..])
                .build();
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, Some(Version::H2));
        }
    }

    #[tokio::test]
    async fn http1() {
        let _ = tracing_subscriber::fmt::try_init();

        for i in 1..HTTP11_LINE.len() {
            debug!(read0 = ?std::str::from_utf8(&HTTP11_LINE[..i]).unwrap());
            debug!(read1 = ?std::str::from_utf8(&HTTP11_LINE[i..]).unwrap());
            let mut buf = BytesMut::with_capacity(1024);
            let mut io = io::Builder::new()
                .read(&HTTP11_LINE[..i])
                .read(&HTTP11_LINE[i..])
                .build();
            let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
            assert_eq!(kind, Some(Version::Http1));
        }

        const REQ: &'static [u8] =
            b"GET /foo/bar/bar/blah HTTP/1.1\r\nHost: foob.example.com\r\n\r\n";
        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(&REQ).build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, Some(Version::Http1));
        assert_eq!(&buf[..], REQ);
    }

    #[tokio::test]
    async fn unknown() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new()
            .read(b"foo.bar.blah\r")
            .read(b"\nbobo")
            .build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, None);
        assert_eq!(&buf[..], b"foo.bar.blah\r\nbobo");

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new().read(GARBAGE).build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, None);
        assert_eq!(&buf[..], GARBAGE);

        let mut buf = BytesMut::with_capacity(1024);
        let mut io = io::Builder::new()
            .read(&HTTP11_LINE[..14])
            .read(b"\n")
            .build();
        let kind = DetectHttp(()).detect(&mut io, &mut buf).await.unwrap();
        assert_eq!(kind, None);
        assert_eq!(&buf[..14], &HTTP11_LINE[..14]);
        assert_eq!(&buf[14..], b"\n");
    }
}
