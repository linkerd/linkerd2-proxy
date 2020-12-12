use crate::Version;
use bytes::BytesMut;
use futures::prelude::*;
use linkerd2_error::Error;
use linkerd2_io::{self as io, AsyncReadExt};
use linkerd2_proxy_transport::{Detect, NewDetectService};
use linkerd2_stack::layer;
use tokio::time;
use tracing::{debug, trace};

const BUFFER_CAPACITY: usize = 8192;
const H2_PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_FIRST_LINE_LEN: usize = 16;

#[inline]
fn is_h2_first_line(line: &[u8]) -> bool {
    line.len() == H2_FIRST_LINE_LEN && line == &H2_PREFACE[..H2_FIRST_LINE_LEN]
}

#[derive(Clone, Debug)]
pub struct DetectHttp {
    capacity: usize,
    timeout: time::Duration,
}

impl DetectHttp {
    pub fn new(timeout: time::Duration) -> Self {
        Self {
            timeout,
            capacity: BUFFER_CAPACITY,
        }
    }

    pub fn layer<N>(
        timeout: time::Duration,
    ) -> impl layer::Layer<N, Service = NewDetectService<N, Self>> + Clone {
        NewDetectService::layer(Self::new(timeout))
    }
}

#[async_trait::async_trait]
impl<I> Detect<I> for DetectHttp
where
    I: io::AsyncRead + Send + Unpin + 'static,
{
    type Kind = Option<Version>;

    async fn detect(&self, mut io: I) -> Result<(Option<Version>, io::PrefixedIo<I>), Error> {
        let mut buf = BytesMut::with_capacity(self.capacity);
        let mut scan_idx = 0;
        let mut maybe_h1 = true;
        let mut maybe_h2 = true;

        // Start tracking a detection timeout before reading.
        let mut timeout = time::sleep(self.timeout).map(|_| self.timeout).fuse();
        loop {
            // Read data from the socket or timeout detection.
            trace!(
                capacity = buf.capacity(),
                scan_idx,
                maybe_h1,
                maybe_h2,
                "Reading"
            );
            futures::select_biased! {
                res = io.read_buf(&mut buf).fuse() => {
                    if res? == 0 {
                        // No data was read because the socket closed or the
                        // buffer capacity was exhausted.
                        debug!(read = buf.len(), "Could not detect protocol");
                        return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                    }
                }
                timeout = (&mut timeout) => {
                    debug!(?timeout, "Detection timeout");
                    return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                }
            }

            // HTTP/2 checking is faster because it's a simple string match. If
            // we have enough data, check it first. In almost all cases, the
            // whole preface should be available from the first read.
            if maybe_h2 && buf.len() >= H2_PREFACE.len() {
                trace!("Checking H2 preface");
                if &buf[..H2_PREFACE.len()] == H2_PREFACE {
                    trace!("Matched HTTP/2 prefix");
                    return Ok((Some(Version::H2), io::PrefixedIo::new(buf.freeze(), io)));
                }

                // Not a match. Don't check for an HTTP/2 preface again.
                maybe_h2 = false;
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
                        if !is_h2_first_line(&buf[..=i + 1]) {
                            trace!("Parsing HTTP/1 message");
                            // If we get to reading headers (and fail), the
                            // first line looked like an HTTP/1 request; so
                            // handle the stream as HTTP/1.
                            if let Ok(_) | Err(httparse::Error::TooManyHeaders) =
                                httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0])
                                    .parse(&buf[..])
                            {
                                trace!("Matched HTTP/1");
                                return Ok((
                                    Some(Version::Http1),
                                    io::PrefixedIo::new(buf.freeze(), io),
                                ));
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
                return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
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
    const TIMEOUT: time::Duration = time::Duration::from_secs(10);
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
            let (kind, _) = DetectHttp::new(TIMEOUT)
                .detect(
                    io::Builder::new()
                        .read(&H2_AND_GARBAGE[..i])
                        .read(&H2_AND_GARBAGE[i..])
                        .build(),
                )
                .await
                .unwrap();
            assert_eq!(kind, Some(Version::H2));
        }
    }

    #[tokio::test]
    async fn http1() {
        let _ = tracing_subscriber::fmt::try_init();

        for i in 1..HTTP11_LINE.len() {
            debug!(read0 = ?std::str::from_utf8(&HTTP11_LINE[..i]).unwrap());
            debug!(read1 = ?std::str::from_utf8(&HTTP11_LINE[i..]).unwrap());
            let (kind, _) = DetectHttp::new(TIMEOUT)
                .detect(
                    io::Builder::new()
                        .read(&HTTP11_LINE[..i])
                        .read(&HTTP11_LINE[i..])
                        .build(),
                )
                .await
                .unwrap();
            assert_eq!(kind, Some(Version::Http1));
        }

        const REQ: &'static [u8] =
            b"GET /foo/bar/bar/blah HTTP/1.1\r\nHost: foob.example.com\r\n\r\n";
        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(&REQ).build())
            .await
            .unwrap();
        assert_eq!(kind, Some(Version::Http1));
        assert_eq!(io.prefix(), REQ);
    }

    #[tokio::test]
    async fn unknown() {
        let _ = tracing_subscriber::fmt::try_init();

        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(
                io::Builder::new()
                    .read(b"foo.bar.blah\r")
                    .read(b"\nbobo")
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], b"foo.bar.blah\r\nbobo");

        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(io::Builder::new().read(GARBAGE).build())
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], GARBAGE);

        let (kind, io) = DetectHttp::new(TIMEOUT)
            .detect(
                io::Builder::new()
                    .read(&HTTP11_LINE[..14])
                    .read(b"\n")
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..14], &HTTP11_LINE[..14]);
        assert_eq!(&io.prefix()[14..], b"\n");
    }

    #[tokio::test]
    async fn timeout() {
        let (io, _handle) = io::Builder::new().read(b"GET").build_with_handle();
        let (kind, io) = DetectHttp::new(time::Duration::from_millis(1))
            .detect(io)
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], b"GET");
    }
}
