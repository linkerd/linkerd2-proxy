
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
