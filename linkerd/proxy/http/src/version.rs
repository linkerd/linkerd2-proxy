use thiserror::Error;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum Version {
    Http1,
    H2,
}

#[derive(Debug, Error)]
#[error("unsupported HTTP version {:?}", self.0)]
pub struct Unsupported(http::Version);

impl Version {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http1 => "HTTP/1",
            Self::H2 => "HTTP/2",
        }
    }
}

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

impl std::fmt::Debug for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http1 => write!(f, "HTTP/1"),
            Self::H2 => write!(f, "HTTP/2"),
        }
    }
}
