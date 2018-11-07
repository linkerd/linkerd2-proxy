use http;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use convert::TryFrom;
use dns;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum HostPort {
    Name(NamePort),
    Addr(SocketAddr),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NamePort {
    name: dns::Name,
    port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    /// The host is not a valid DNS name or IP address.
    InvalidHost,

    /// The port is missing.
    MissingPort,
}

// === impl HostAndPort ===

impl HostPort {
    pub fn new(host: &str, port: u16) -> Result<Self, Error> {
        match IpAddr::from_str(host) {
            Ok(ip) => Ok(HostPort::Addr((ip, port).into())),
            Err(_) => NamePort::new(host, port).map(HostPort::Name),
        }
    }

    pub fn from_authority_and_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::new(a.host(), a.port().unwrap_or(default_port))
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .ok_or(Error::MissingPort)
            .and_then(|p| Self::new(a.host(), p))
    }

    pub fn port(&self) -> u16 {
        match self {
            HostPort::Name(n) => n.port(),
            HostPort::Addr(a) => a.port(),
        }
    }

    pub fn is_loopback(&self) -> bool {
        match self {
            HostPort::Name(n) => n.is_localhost(),
            HostPort::Addr(a) => a.ip().is_loopback(),
        }
    }

    pub fn as_authority(&self) -> http::uri::Authority {
        let s = format!("{}", self);
        http::uri::Authority::from_str(&s).expect("HostPort must render as valid authority")
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        match self {
            HostPort::Addr(a) => Some(*a),
            HostPort::Name(_) => None,
        }
    }

    pub fn name(&self) -> Option<&NamePort> {
        match self {
            HostPort::Name(ref n) => Some(n),
            HostPort::Addr(_) => None,
        }
    }

    pub fn into_name(self) -> Option<NamePort> {
        match self {
            HostPort::Name(n) => Some(n),
            HostPort::Addr(_) => None,
        }
    }
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HostPort::Name(NamePort { ref name, port }) => write!(f, "{}:{}", name, port),
            HostPort::Addr(addr) => write!(f, "{}", addr),
        }
    }
}

// === impl NamePort ===

impl NamePort {
    pub fn new(host: &str, port: u16) -> Result<Self, Error> {
        if host.is_empty() {
            return Err(Error::InvalidHost);
        }

        dns::Name::try_from(host.as_bytes())
            .map(|name| NamePort { name, port })
            .map_err(|_| Error::InvalidHost)
    }

    pub fn from_authority_with_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::new(a.host(), a.port().unwrap_or(default_port))
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .ok_or(Error::MissingPort)
            .and_then(|p| Self::new(a.host(), p))
    }

    pub fn name(&self) -> &dns::Name {
        &self.name
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_localhost(&self) -> bool {
        self.name.is_localhost()
    }
}

impl fmt::Display for NamePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::uri::Authority;

    #[test]
    fn test_is_loopback() {
        let cases = &[
            ("localhost", false), // Not absolute
            ("localhost.", true),
            ("LocalhOsT.", true),   // Case-insensitive
            ("mlocalhost.", false), // prefixed
            ("localhost1.", false), // suffixed
            ("127.0.0.1", true),    // IPv4
            ("[::1]", true),        // IPv6
        ];
        for (host, expected_result) in cases {
            let authority = Authority::from_static(host);
            let hp = HostPort::from_authority_and_default_port(&authority, 80).unwrap();
            assert_eq!(hp.is_loopback(), *expected_result, "{:?}", host)
        }
    }
}
