use tracing::{debug, trace};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Version {
    Http1,
    H2,
}

#[derive(Debug)]
pub struct Unsupported(http::Version);

impl std::convert::TryFrom<http::Version> for Version {
    type Error = Unsupported;
    fn try_from(v: http::Version) -> Result<Self, Unsupported> {
        match v {
            http::Version::HTTP_10 | http::Version::HTTP_11 => Ok(Self::Http1),
            http::Version::HTTP_2 => Ok(Self::H2),
            v => Err(Unsupported(v)),
        }
    }
}

impl Version {
    const H2_PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    pub const DETECT_BUFFER_CAPACITY: usize = 8192;

    /// Tries to detect a known protocol in the peeked bytes.
    ///
    /// If no protocol can be determined, returns `None`.
    pub fn from_prefix(bytes: &[u8]) -> Option<Self> {
        // http2 is easiest to detect
        if bytes.len() >= Self::H2_PREFACE.len() {
            if &bytes[..Self::H2_PREFACE.len()] == Self::H2_PREFACE {
                trace!("Detected H2");
                return Some(Self::H2);
            }
        }

        // http1 can have a really long first line, but if the bytes so far
        // look like http1, we'll assume it is. a different protocol
        // should look different in the first few bytes

        let mut headers = [httparse::EMPTY_HEADER; 0];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(bytes) {
            // Ok(Complete) or Ok(Partial) both mean it looks like HTTP1!
            //
            // If we got past the first line, we'll see TooManyHeaders,
            // because we passed an array of 0 headers to parse into. That's fine!
            // We didn't want to keep parsing headers, just validate that
            // the first line is HTTP1.
            Ok(_) | Err(httparse::Error::TooManyHeaders) => {
                trace!("Detected H1");
                return Some(Self::Http1);
            }
            _ => {}
        }

        debug!("Not HTTP");
        trace!(?bytes);
        None
    }
}

// A convenience for tracing contexts.
impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http1 => write!(f, "1.x"),
            Self::H2 => write!(f, "h2"),
        }
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unsupported HTTP version")
    }
}

impl std::error::Error for Unsupported {}

#[cfg(test)]
#[test]
fn from_prefix() {
    assert_eq!(Version::from_prefix(Version::H2_PREFACE), Some(Version::H2));
    assert_eq!(
        Version::from_prefix("GET /foo/bar/bah/baz HTTP/1.1".as_ref()),
        Some(Version::Http1)
    );
    assert_eq!(
        Version::from_prefix("GET /foo".as_ref()),
        Some(Version::Http1)
    );
    assert_eq!(
        Version::from_prefix("GET /foo/barbasdklfja\n".as_ref()),
        None
    );
}
