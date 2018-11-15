use http;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use convert::TryFrom;
pub use dns::Name;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Addr {
    Name(NameAddr),
    Socket(SocketAddr),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NameAddr {
    name: Name,
    port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    /// The host is not a valid DNS name or IP address.
    InvalidHost,

    /// The port is missing.
    MissingPort,
}

// === impl Addr ===

impl Addr {
    pub fn from_str(hostport: &str) -> Result<Self, Error> {
        SocketAddr::from_str(hostport)
            .map(Addr::Socket)
            .or_else(|_| NameAddr::from_str(hostport).map(Addr::Name))
    }

    pub fn from_str_and_port(host: &str, port: u16) -> Result<Self, Error> {
        IpAddr::from_str(host)
            .map(|ip| Addr::Socket((ip, port).into()))
            .or_else(|_| NameAddr::from_str_and_port(host, port).map(Addr::Name))
    }

    pub fn from_authority_and_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::from_str_and_port(a.host(), a.port().unwrap_or(default_port))
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .ok_or(Error::MissingPort)
            .and_then(|p| Self::from_str_and_port(a.host(), p))
    }

    pub fn port(&self) -> u16 {
        match self {
            Addr::Name(n) => n.port(),
            Addr::Socket(a) => a.port(),
        }
    }

    pub fn is_loopback(&self) -> bool {
        match self {
            Addr::Name(n) => n.is_localhost(),
            Addr::Socket(a) => a.ip().is_loopback(),
        }
    }

    pub fn as_authority(&self) -> http::uri::Authority {
        match self {
            Addr::Name(n) => n.as_authority(),
            Addr::Socket(a) => http::uri::Authority::from_str(&format!("{}", a))
                .expect("SocketAddr must be valid authority"),
        }
    }

    pub fn socket_addr(&self) -> Option<SocketAddr> {
        match self {
            Addr::Socket(a) => Some(*a),
            Addr::Name(_) => None,
        }
    }

    pub fn name_addr(&self) -> Option<&NameAddr> {
        match self {
            Addr::Name(ref n) => Some(n),
            Addr::Socket(_) => None,
        }
    }

    pub fn into_name_addr(self) -> Option<NameAddr> {
        match self {
            Addr::Name(n) => Some(n),
            Addr::Socket(_) => None,
        }
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Addr::Name(name) => name.fmt(f),
            Addr::Socket(addr) => addr.fmt(f),
        }
    }
}

impl From<NameAddr> for Addr {
    fn from(na: NameAddr) -> Self {
        Addr::Name(na)
    }
}

// === impl NameAddr ===

impl NameAddr {
    pub fn new(name: Name, port: u16) -> Self {
        NameAddr { name, port }
    }

    pub fn from_str(hostport: &str) -> Result<Self, Error> {
        let mut parts = hostport.rsplitn(2, ':');
        let port = parts
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or(Error::MissingPort)?;
        let host = parts.next().ok_or(Error::InvalidHost)?;
        Self::from_str_and_port(host, port)
    }

    pub fn from_str_and_port(host: &str, port: u16) -> Result<Self, Error> {
        if host.is_empty() {
            return Err(Error::InvalidHost);
        }

        Name::try_from(host.as_bytes())
            .map(|name| NameAddr { name, port })
            .map_err(|_| Error::InvalidHost)
    }

    pub fn from_authority_with_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::from_str_and_port(a.host(), a.port().unwrap_or(default_port))
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .ok_or(Error::MissingPort)
            .and_then(|p| Self::from_str_and_port(a.host(), p))
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_localhost(&self) -> bool {
        self.name.is_localhost()
    }

    pub fn as_authority(&self) -> http::uri::Authority {
        http::uri::Authority::from_str(&format!("{}", self))
            .expect("NameAddr must be valid authority")
    }
}

impl fmt::Display for NameAddr {

    fn fmt(&self, f: &mut fmt::Formatter)  -> fmt::Result {
        write!(f, "{}:{}", self.name.without_trailing_dot(), self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_loopback() {
        let cases = &[
            ("localhost:80", false), // Not absolute
            ("localhost.:80", true),
            ("LocalhOsT.:80", true),   // Case-insensitive
            ("mlocalhost.:80", false), // prefixed
            ("localhost1.:80", false), // suffixed
            ("127.0.0.1:80", true),    // IPv4
            ("[::1]:80", true),        // IPv6
        ];
        for (host, expected_result) in cases {
            let a = Addr::from_str(host).unwrap();
            assert_eq!(a.is_loopback(), *expected_result, "{:?}", host)
        }
    }
}
