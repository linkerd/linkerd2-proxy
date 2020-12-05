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
        let mut bufidx = 0;
        let mut maybe_h1 = true;
        let mut maybe_h2 = true;

        // Start tracking a detection timeout before reading.
        let mut timeout = time::sleep(self.timeout).fuse();
        loop {
            if !(maybe_h1 || maybe_h2) {
                trace!("Unknown protocol");
                return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
            }

            // Read data from the socket or timeout detection.
            trace!(capacity = buf.capacity() - bufidx, "Reading");
            let sz = futures::select_biased! {
                res = io.read_buf(&mut buf).fuse() => {
                    let sz = res?;
                    if sz == 0 {
                        debug!(read = buf.len(), "Could not detect protocol");
                        return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                    }
                    sz
                }
                _ = (&mut timeout) => {
                    debug!(ms = %self.timeout.as_millis(), "Detection timeout");
                    return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                }
            };
            // If no data was read

            // If we've read enough
            if maybe_h2 && buf.len() >= H2_PREFACE.len() {
                trace!(
                    buf = buf.len(),
                    h2 = H2_PREFACE.len(),
                    "Checking H2 preface"
                );
                if &buf[..H2_PREFACE.len()] == H2_PREFACE {
                    trace!("Matched HTTP/2 prefix");
                    return Ok((Some(Version::H2), io::PrefixedIo::new(buf.freeze(), io)));
                }
                // If the buffer was long enough
                maybe_h2 = false;
            }

            // Scan newly-read bytes for a line ending
            if maybe_h1 {
                for j in bufidx..(buf.len() - 1) {
                    // If we've reached the end of a line, we have enough
                    // information to know whether the protocol is HTTP/1.1 or not.
                    if &buf[j..j + 2] == b"\r\n" {
                        trace!(offset = j, "Found newline");
                        if is_h2_first_line(&buf[..j + 2]) {
                            maybe_h1 = false;
                            break;
                        } else {
                            trace!("Atempt to parse HTTP/1 message");
                            let mut p = httparse::Request::new(&mut [httparse::EMPTY_HEADER; 0]);
                            // Check whether the first line looks like HTTP/1.1.
                            match p.parse(&buf[..]) {
                                Ok(_) | Err(httparse::Error::TooManyHeaders) => {
                                    trace!("Matched HTTP/1");
                                    return Ok((
                                        Some(Version::Http1),
                                        io::PrefixedIo::new(buf.freeze(), io),
                                    ));
                                }
                                Err(_) => {
                                    if !maybe_h2 {
                                        trace!("Unknown protocol");
                                        return Ok((None, io::PrefixedIo::new(buf.freeze(), io)));
                                    }
                                    maybe_h1 = false;
                                }
                            };
                        }
                    }
                }
            }

            bufidx += sz - 1;
            if buf[bufidx] == b'\r' {
                bufidx -= 1;
            }
            trace!(offset = %bufidx, "Continuing to read");
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

    #[tokio::test]
    async fn h2() {
        let _ = tracing_subscriber::fmt::try_init();

        for i in 1..H2_PREFACE.len() {
            let mut buf = BytesMut::with_capacity(H2_PREFACE.len());
            buf.put(H2_PREFACE);
            debug!(read0 = ?std::str::from_utf8(&H2_PREFACE[0..i]).unwrap());
            debug!(read1 = ?std::str::from_utf8(&H2_PREFACE[i..]).unwrap());
            let (kind, _) = DetectHttp::new(TIMEOUT)
                .detect(
                    io::Builder::new()
                        .read(&H2_PREFACE[..i])
                        .read(&H2_PREFACE[i..])
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
            debug!(read0 = ?std::str::from_utf8(&HTTP11_LINE[0..i]).unwrap());
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
            .detect(io::Builder::new().read(b"foo.bar.blah\n").build())
            .await
            .unwrap();
        assert_eq!(kind, None);
        assert_eq!(&io.prefix()[..], b"foo.bar.blah\n");

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
