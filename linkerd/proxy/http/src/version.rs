use thiserror::Error;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Version {
    Http1,
    H2,
}

#[derive(Debug, Error)]
#[error("unsupported HTTP version {:?}", self.0)]
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
