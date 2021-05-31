#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]
use linkerd_dns_name::Name;
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};
use thiserror::Error;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Error {
    /// The host is not a valid DNS name or IP address.
    #[error("address contains an invalid host")]
    InvalidHost,

    /// The port is missing.
    #[error("address is missing a port")]
    MissingPort,
}

// === impl Addr ===

impl FromStr for Addr {
    type Err = Error;

    fn from_str(hostport: &str) -> Result<Self, Error> {
        SocketAddr::from_str(hostport)
            .map(Addr::Socket)
            .or_else(|_| NameAddr::from_str(hostport).map(Addr::Name))
    }
}

impl Addr {
    pub fn from_str_and_port(host: &str, port: u16) -> Result<Self, Error> {
        IpAddr::from_str(host)
            .map(|ip| Addr::Socket((ip, port).into()))
            .or_else(|_| NameAddr::from_str_and_port(host, port).map(Addr::Name))
    }

    pub fn from_authority_and_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::from_str_and_port(
            a.host(),
            a.port().map(|p| p.as_u16()).unwrap_or(default_port),
        )
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .map(|p| p.as_u16())
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

    pub fn to_http_authority(&self) -> http::uri::Authority {
        match self {
            Addr::Name(n) => n.as_http_authority(),
            Addr::Socket(ref a) if a.port() == 80 => {
                let ip = if a.is_ipv4() {
                    a.ip().to_string()
                } else {
                    // When IPv6 or later addresses are used in an authority,
                    // they must be within square brackets. See
                    // https://tools.ietf.org/html/rfc3986#section-3.2 for
                    // details. The `fmt::Display` implementation of the
                    // `Ipv6Addr` type does not include brackets, so we must add
                    // them ourselves.
                    format!("[{}]", a.ip())
                };
                http::uri::Authority::from_str(&ip).unwrap_or_else(|err| {
                    panic!("SocketAddr ({}) must be valid authority: {}", a, err)
                })
            }
            Addr::Socket(a) => {
                http::uri::Authority::from_str(&a.to_string()).unwrap_or_else(|err| {
                    panic!("SocketAddr ({}) must be valid authority: {}", a, err)
                })
            }
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::Name(name) => name.fmt(f),
            Addr::Socket(addr) => addr.fmt(f),
        }
    }
}

impl From<SocketAddr> for Addr {
    fn from(sa: SocketAddr) -> Self {
        Addr::Socket(sa)
    }
}

impl From<NameAddr> for Addr {
    fn from(na: NameAddr) -> Self {
        Addr::Name(na)
    }
}

impl From<(Name, u16)> for Addr {
    fn from((n, p): (Name, u16)) -> Self {
        Addr::Name((n, p).into())
    }
}

impl AsRef<Self> for Addr {
    fn as_ref(&self) -> &Self {
        self
    }
}

// === impl NameAddr ===

impl From<(Name, u16)> for NameAddr {
    fn from((name, port): (Name, u16)) -> Self {
        NameAddr { name, port }
    }
}

impl FromStr for NameAddr {
    type Err = Error;

    fn from_str(hostport: &str) -> Result<Self, Error> {
        let mut parts = hostport.rsplitn(2, ':');
        let port = parts
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or(Error::MissingPort)?;
        let host = parts.next().ok_or(Error::InvalidHost)?;
        Self::from_str_and_port(host, port)
    }
}

impl NameAddr {
    pub fn from_str_and_port(host: &str, port: u16) -> Result<Self, Error> {
        if host.is_empty() {
            return Err(Error::InvalidHost);
        }

        Name::from_str(host)
            .map(|name| NameAddr { name, port })
            .map_err(|_| Error::InvalidHost)
    }

    pub fn from_authority_with_default_port(
        a: &http::uri::Authority,
        default_port: u16,
    ) -> Result<Self, Error> {
        Self::from_str_and_port(
            a.host(),
            a.port().map(|p| p.as_u16()).unwrap_or(default_port),
        )
    }

    pub fn from_authority_with_port(a: &http::uri::Authority) -> Result<Self, Error> {
        a.port()
            .map(|p| p.as_u16())
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

    pub fn as_http_authority(&self) -> http::uri::Authority {
        if self.port == 80 {
            http::uri::Authority::from_str(self.name.without_trailing_dot())
                .expect("NameAddr must be valid authority")
        } else {
            http::uri::Authority::from_str(&self.to_string())
                .expect("NameAddr must be valid authority")
        }
    }
}

impl fmt::Display for NameAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

    fn test_to_http_authority(cases: &[&str]) {
        let width = cases.iter().map(|s| s.len()).max().unwrap_or(0);
        for host in cases {
            print!("trying {:1$} ... ", host, width);
            Addr::from_str(host).unwrap().to_http_authority();
            println!("ok");
        }
    }

    #[test]
    fn to_http_authority_names_port_80() {
        test_to_http_authority(&[
            "localhost:80",
            "localhost.:80",
            "LocalhOsT.:80",
            "mlocalhost.:80",
            "localhost1.:80",
        ])
    }

    #[test]
    fn to_http_authority_names() {
        test_to_http_authority(&[
            "localhost:9090",
            "localhost.:9090",
            "LocalhOsT.:9090",
            "mlocalhost.:9090",
            "localhost1.:9090",
        ])
    }

    #[test]
    fn to_http_authority_ipv4_port_80() {
        test_to_http_authority(&["10.7.0.42:80", "127.0.0.1:80"])
    }

    #[test]
    fn to_http_authority_ipv4() {
        test_to_http_authority(&["10.7.0.42:9090", "127.0.0.1:9090"])
    }

    #[test]
    fn to_http_authority_ipv6_port_80() {
        test_to_http_authority(&[
            "[2001:0db8:0000:0000:0000:8a2e:0370:7334]:80",
            "[2001:db8::8a2e:370:7334]:80",
            "[::1]:80",
        ])
    }

    #[test]
    fn to_http_authority_ipv6() {
        test_to_http_authority(&[
            "[2001:0db8:0000:0000:0000:8a2e:0370:7334]:9090",
            "[2001:db8::8a2e:370:7334]:9090",
            "[::1]:9090",
        ])
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;

    pub fn fuzz_addr_1(fuzz_data: &str) {
        if let Ok(addr) = Addr::from_str(fuzz_data) {
            addr.is_loopback();
            addr.to_http_authority();
            addr.is_loopback();
            addr.socket_addr();
        }

        if let Ok(name_addr) = NameAddr::from_str_and_port(fuzz_data, 1234) {
            name_addr.port();
            name_addr.as_http_authority();
        }
    }
}
