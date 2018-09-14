use http;
use std::fmt;
use std::net::SocketAddr;

use proxy::{
    http::{h1, Settings},
    Source,
};
use transport::{DnsNameAndPort, Host, HostAndPort};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Destination {
    pub name_or_addr: NameOrAddr,
    pub settings: Settings,
    _p: (),
}

/// Describes a destination for HTTP requests.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NameOrAddr {
    /// A logical, lazily-bound endpoint.
    Name(DnsNameAndPort),

    /// A single, bound endpoint.
    Addr(SocketAddr),
}

impl Destination {
    pub fn new(
        name_or_addr: NameOrAddr,
        settings: Settings
    ) -> Self {
        Self {
            name_or_addr,
            settings,
            _p: (),
        }
    }

    pub fn from_request<A>(req: &http::Request<A>) -> Option<Self> {
        let name_or_addr = NameOrAddr::from_request(req)?;
       let settings = Settings::detect(req);
        Some(Self::new(name_or_addr, settings))
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.name_or_addr.fmt(f)
    }
}

impl fmt::Display for NameOrAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NameOrAddr::Name(ref name) => write!(f, "{}:{}", name.host, name.port),
            NameOrAddr::Addr(ref addr) => addr.fmt(f),
        }
    }
}

impl NameOrAddr {

    /// Determines the destination for a request.
    ///
    /// Typically, a request's authority is used to produce a `NameOrAddr`. If the
    /// authority addresses a DNS name, a `NameOrAddr::Name` is returned; and, otherwise,
    /// it addresses a fixed IP address and a `NameOrAddr::Addr` is returned. The port is
    /// inferred if not specified in the authority.
    ///
    /// If no authority is available, the `SO_ORIGINAL_DST` socket option is checked. If
    /// it's available, it is used to return a `NameOrAddr::Addr`. This socket option is
    /// typically set by `iptables(8)` in containerized environments like Kubernetes (as
    /// configured by the `proxy-init` program).
    ///
    /// If none of this information is available, no `NameOrAddr` is returned.
    pub fn from_request<B>(req: &http::Request<B>) -> Option<NameOrAddr> {
        match Self::host_port(req) {
            Some(HostAndPort {
                host: Host::DnsName(host),
                port,
            }) => {
                let name_or_addr = DnsNameAndPort { host, port };
                Some(NameOrAddr::Name(name_or_addr))
            }

            Some(HostAndPort {
                host: Host::Ip(ip),
                port,
            }) => {
                let name_or_addr = SocketAddr::from((ip, port));
                Some(NameOrAddr::Addr(name_or_addr))
            }

            None => req
                .extensions()
                .get::<Source>()
                .and_then(|src| src.orig_dst_if_not_local())
                .map(NameOrAddr::Addr),
        }
    }

    /// Determines the logical host:port of the request.
    ///
    /// If the parsed URI includes an authority, use that. Otherwise, try to load the
    /// authority from the `Host` header.
    ///
    /// The port is either parsed from the authority or a default of 80 is used.
    fn host_port<B>(req: &http::Request<B>) -> Option<HostAndPort> {
        // Note: Calls to `normalize` cannot be deduped without cloning `authority`.
        req.uri()
            .authority_part()
            .and_then(Self::normalize)
            .or_else(|| h1::authority_from_host(req).and_then(|h| Self::normalize(&h)))
    }

    /// TODO: Return error when `HostAndPort::normalize()` fails.
    /// TODO: Use scheme-appropriate default port.
    fn normalize(authority: &http::uri::Authority) -> Option<HostAndPort> {
        const DEFAULT_PORT: Option<u16> = Some(80);
        HostAndPort::normalize(authority, DEFAULT_PORT).ok()
    }
}
