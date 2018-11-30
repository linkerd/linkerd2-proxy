use http;
use indexmap::IndexMap;
use ipnet::{Contains, Ipv4Net, Ipv6Net};
use std::boxed::Box;
use std::net;
use std::{error, fmt};

use api::net::ip_address;
use api::tap::observe_request;
use convert::TryFrom;

use tap::Inspect;

#[derive(Clone, Debug)]
pub enum Match {
    Any(Vec<Match>),
    All(Vec<Match>),
    Not(Box<Match>),
    Source(TcpMatch),
    Destination(TcpMatch),
    DestinationLabel(LabelMatch),
    Http(HttpMatch),
}

#[derive(Debug, Eq, PartialEq)]
pub enum InvalidMatch {
    Empty,
    InvalidPort,
    InvalidNetwork,
    InvalidHttpMethod,
    InvalidScheme,
}

#[derive(Clone, Debug)]
pub struct LabelMatch {
    key: String,
    value: String,
}

#[derive(Clone, Debug)]
pub enum TcpMatch {
    // Inclusive
    PortRange(u16, u16),
    Net(NetMatch),
}

#[derive(Clone, Debug)]
pub enum NetMatch {
    Net4(Ipv4Net),
    Net6(Ipv6Net),
}

#[derive(Clone, Debug)]
pub enum HttpMatch {
    Scheme(http::uri::Scheme),
    Method(http::Method),
    Path(observe_request::match_::http::string_match::Match),
    Authority(observe_request::match_::http::string_match::Match),
}

// ===== impl Match ======

impl Match {
    fn from_seq(seq: observe_request::match_::Seq) -> Result<Vec<Self>, InvalidMatch> {
        let mut new = Vec::with_capacity(seq.matches.len());
        for m in seq.matches.into_iter().filter_map(|m| m.match_) {
            new.push(Self::try_from(m)?);
        }

        Ok(new)
    }

    pub fn matches<B, I: Inspect>(&self, req: &http::Request<B>, inspect: &I) -> bool {
        match self {
            Match::Any(ref ms) => ms.iter().any(|m| m.matches(req, inspect)),
            Match::All(ref ms) => ms.iter().all(|m| m.matches(req, inspect)),
            Match::Not(ref not) => !not.matches(req, inspect),
            Match::Source(ref src) => inspect
                .src_addr(req)
                .map(|s| src.matches(s))
                .unwrap_or(false),
            Match::Destination(ref dst) => inspect
                .dst_addr(req)
                .map(|d| dst.matches(d))
                .unwrap_or(false),
            Match::DestinationLabel(ref lbl) => inspect
                .dst_labels(req)
                .map(|l| lbl.matches(l))
                .unwrap_or(false),
            Match::Http(ref http) => http.matches(req, inspect),
        }
    }
}

impl Match {
    pub fn try_new(m: Option<observe_request::Match>) -> Result<Self, InvalidMatch> {
        m.and_then(|m| m.match_)
            .map(Self::try_from)
            .unwrap_or_else(|| Err(InvalidMatch::Empty))
    }
}

impl TryFrom<observe_request::match_::Match> for Match {
    type Err = InvalidMatch;

    #[allow(unconditional_recursion)]
    fn try_from(m: observe_request::match_::Match) -> Result<Self, Self::Err> {
        use api::tap::observe_request::match_;

        match m {
            match_::Match::All(seq) => Self::from_seq(seq).map(Match::All),
            match_::Match::Any(seq) => Self::from_seq(seq).map(Match::Any),
            match_::Match::Not(m) => m
                .match_
                .ok_or(InvalidMatch::Empty)
                .and_then(Self::try_from)
                .map(|m| Match::Not(Box::new(m))),
            match_::Match::Source(src) => TcpMatch::try_from(src).map(Match::Source),
            match_::Match::Destination(dst) => TcpMatch::try_from(dst).map(Match::Destination),
            match_::Match::DestinationLabel(l) => {
                LabelMatch::try_from(l).map(Match::DestinationLabel)
            }
            match_::Match::Http(http) => HttpMatch::try_from(http).map(Match::Http),
        }
    }
}

// ===== impl LabelMatch ======

impl LabelMatch {
    fn matches(&self, labels: &IndexMap<String, String>) -> bool {
        labels.get(&self.key) == Some(&self.value)
    }
}

impl TryFrom<observe_request::match_::Label> for LabelMatch {
    type Err = InvalidMatch;

    fn try_from(m: observe_request::match_::Label) -> Result<Self, InvalidMatch> {
        if m.key.is_empty() || m.value.is_empty() {
            return Err(InvalidMatch::Empty);
        }

        Ok(LabelMatch {
            key: m.key.clone(),
            value: m.value.clone(),
        })
    }
}

// ===== impl TcpMatch ======

impl TcpMatch {
    fn matches(&self, addr: net::SocketAddr) -> bool {
        match self {
            // If either a minimum or maximum is not specified, the range is considered to
            // be over a discrete value.
            TcpMatch::PortRange(min, max) => *min <= addr.port() && addr.port() <= *max,
            TcpMatch::Net(net) => net.matches(&addr.ip()),
        }
    }
}

impl TryFrom<observe_request::match_::Tcp> for TcpMatch {
    type Err = InvalidMatch;

    fn try_from(m: observe_request::match_::Tcp) -> Result<Self, InvalidMatch> {
        use api::tap::observe_request::match_::tcp;

        m.match_.ok_or(InvalidMatch::Empty).and_then(|t| match t {
            tcp::Match::Ports(range) => {
                // If either a minimum or maximum is not specified, the range is considered to
                // be over a discrete value.
                let min = if range.min == 0 { range.max } else { range.min };
                let max = if range.max == 0 { range.min } else { range.max };
                if min == 0 || max == 0 {
                    return Err(InvalidMatch::Empty);
                }
                if min > u32::from(::std::u16::MAX) || max > u32::from(::std::u16::MAX) {
                    return Err(InvalidMatch::InvalidPort);
                }
                Ok(TcpMatch::PortRange(min as u16, max as u16))
            }

            tcp::Match::Netmask(netmask) => NetMatch::try_from(netmask).map(TcpMatch::Net),
        })
    }
}

// ===== impl NetMatch ======

impl NetMatch {
    fn matches(&self, addr: &net::IpAddr) -> bool {
        match self {
            NetMatch::Net4(net) => match addr {
                net::IpAddr::V6(_) => false,
                net::IpAddr::V4(addr) => net.contains(addr),
            },
            NetMatch::Net6(net) => match addr {
                net::IpAddr::V4(_) => false,
                net::IpAddr::V6(addr) => net.contains(addr),
            },
        }
    }
}

impl TryFrom<observe_request::match_::tcp::Netmask> for NetMatch {
    type Err = InvalidMatch;

    fn try_from(m: observe_request::match_::tcp::Netmask) -> Result<Self, InvalidMatch> {
        let mask = if m.mask == 0 {
            return Err(InvalidMatch::Empty);
        } else if m.mask > u32::from(::std::u8::MAX) {
            return Err(InvalidMatch::InvalidNetwork);
        } else {
            m.mask as u8
        };

        let net = match m.ip.and_then(|a| a.ip).ok_or(InvalidMatch::Empty)? {
            ip_address::Ip::Ipv4(n) => {
                let ip = n.into();
                let net = Ipv4Net::new(ip, mask).map_err(|_| InvalidMatch::InvalidNetwork)?;
                NetMatch::Net4(net)
            }
            ip_address::Ip::Ipv6(n) => {
                let ip = (&n).into();
                let net = Ipv6Net::new(ip, mask).map_err(|_| InvalidMatch::InvalidNetwork)?;
                NetMatch::Net6(net)
            }
        };

        Ok(net)
    }
}

// ===== impl HttpMatch ======

impl HttpMatch {
    fn matches<B, I: Inspect>(&self, req: &http::Request<B>, inspect: &I) -> bool {
        match self {
            HttpMatch::Scheme(ref m) => {
                m == req.uri().scheme_part().unwrap_or(&http::uri::Scheme::HTTP)
            }

            HttpMatch::Method(ref m) => m == req.method(),

            HttpMatch::Authority(ref m) => inspect
                .authority(req)
                .map(|a| Self::matches_string(m, &a))
                .unwrap_or(false),

            HttpMatch::Path(ref m) => Self::matches_string(m, req.uri().path()),
        }
    }

    fn matches_string(
        string_match: &observe_request::match_::http::string_match::Match,
        value: &str,
    ) -> bool {
        use api::tap::observe_request::match_::http::string_match::Match::*;

        match string_match {
            Exact(ref exact) => value == exact,
            Prefix(ref prefix) => value.starts_with(prefix),
        }
    }
}

impl TryFrom<observe_request::match_::Http> for HttpMatch {
    type Err = InvalidMatch;
    fn try_from(m: observe_request::match_::Http) -> Result<Self, InvalidMatch> {
        use api::http_types::scheme::{Registered, Type};
        use api::tap::observe_request::match_::http::Match as Pb;

        m.match_.ok_or(InvalidMatch::Empty).and_then(|m| match m {
            Pb::Scheme(s) => s.type_.ok_or(InvalidMatch::Empty).and_then(|s| match s {
                Type::Registered(reg) if reg == Registered::Http.into() => {
                    Ok(HttpMatch::Scheme(http::uri::Scheme::HTTP))
                }
                Type::Registered(reg) if reg == Registered::Https.into() => {
                    Ok(HttpMatch::Scheme(http::uri::Scheme::HTTPS))
                }
                Type::Registered(_) => Err(InvalidMatch::InvalidScheme),
                Type::Unregistered(ref s) => http::uri::Scheme::from_shared(s.as_str().into())
                    .map(HttpMatch::Scheme)
                    .map_err(|_| InvalidMatch::InvalidScheme),
            }),

            Pb::Method(m) => m
                .type_
                .ok_or(InvalidMatch::Empty)
                .and_then(|m| m.try_as_http().map_err(|_| InvalidMatch::InvalidHttpMethod))
                .map(HttpMatch::Method),

            Pb::Authority(a) => a
                .match_
                .ok_or(InvalidMatch::Empty)
                .map(|a| HttpMatch::Authority(a)),

            Pb::Path(p) => p
                .match_
                .ok_or(InvalidMatch::Empty)
                .map(|p| HttpMatch::Path(p)),
        })
    }
}

#[cfg(test)]
mod tests {
    use ipnet::{Contains, Ipv4Net, Ipv6Net};
    use quickcheck::*;
    use std::collections::HashMap;

    use super::*;
    use api::http_types;

    impl Arbitrary for LabelMatch {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            Self {
                key: Arbitrary::arbitrary(g),
                value: Arbitrary::arbitrary(g),
            }
        }
    }

    impl Arbitrary for TcpMatch {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            if g.gen::<bool>() {
                TcpMatch::Net(NetMatch::arbitrary(g))
            } else {
                TcpMatch::PortRange(g.gen(), g.gen())
            }
        }
    }

    impl Arbitrary for NetMatch {
        fn arbitrary<G: Gen>(g: &mut G) -> Self {
            if g.gen::<bool>() {
                let addr = net::Ipv4Addr::arbitrary(g);
                let bits = g.gen::<u8>() % 32;
                let net = Ipv4Net::new(addr, bits).expect("ipv4 network address");
                NetMatch::Net4(net)
            } else {
                let addr = net::Ipv6Addr::arbitrary(g);
                let bits = g.gen::<u8>() % 128;
                let net = Ipv6Net::new(addr, bits).expect("ipv6 network address");
                NetMatch::Net6(net)
            }
        }
    }

    quickcheck! {
        fn tcp_from_proto(tcp: observe_request::match_::Tcp) -> bool {
            use self::observe_request::match_::tcp;

            let err: Option<InvalidMatch> =
                tcp.match_.as_ref()
                    .map(|m| match m {
                        tcp::Match::Ports(ps) => {
                            let ok = 0 < ps.min &&
                                ps.min <= ps.max &&
                                ps.max < u32::from(::std::u16::MAX);
                            if ok { None } else { Some(InvalidMatch::InvalidPort) }
                        }
                        tcp::Match::Netmask(n) => {
                            match n.ip.as_ref().and_then(|ip| ip.ip.as_ref()) {
                                Some(_) => None,
                                None => Some(InvalidMatch::Empty),
                            }
                        }
                    })
                    .unwrap_or(Some(InvalidMatch::Empty));

            err == TcpMatch::try_from(tcp).err()
        }

        fn tcp_matches(m: TcpMatch, addr: net::SocketAddr) -> bool {
            let matches = match (&m, addr.ip()) {
                (&TcpMatch::Net(NetMatch::Net4(ref n)), net::IpAddr::V4(ip)) => {
                    n.contains(&ip)
                }
                (&TcpMatch::Net(NetMatch::Net6(ref n)), net::IpAddr::V6(ip)) => {
                    n.contains(&ip)
                }
                (&TcpMatch::PortRange(min, max), _) => {
                    min <= addr.port() && addr.port() <= max
                }
                _ => false
            };

            m.matches(addr) == matches
        }

        fn labels_from_proto(label: observe_request::match_::Label) -> bool {
            let err: Option<InvalidMatch> =
                if label.key.is_empty() || label.value.is_empty() {
                    Some(InvalidMatch::Empty)
                } else {
                    None
                };

            err == LabelMatch::try_from(label).err()
        }

        fn label_matches(l: LabelMatch, labels: HashMap<String, String>) -> bool {
            use std::iter::FromIterator;

            let matches = labels.get(&l.key) == Some(&l.value);
            l.matches(&IndexMap::from_iter(labels.into_iter())) == matches
        }

        fn http_from_proto(http: observe_request::match_::Http) -> bool {
            use self::observe_request::match_::http;

            let err = match http.match_.as_ref() {
                None => Some(InvalidMatch::Empty),
                Some(http::Match::Method(ref m)) => {
                    match m.type_.as_ref() {
                        None => Some(InvalidMatch::Empty),
                        Some(http_types::http_method::Type::Unregistered(ref m)) if m.len() > 15 => {
                            Some(InvalidMatch::InvalidHttpMethod)
                        }
                        Some(http_types::http_method::Type::Unregistered(m)) => {
                            ::http::Method::from_bytes(m.as_bytes())
                                .err()
                                .map(|_| InvalidMatch::InvalidHttpMethod)
                        }
                        Some(http_types::http_method::Type::Registered(m)) if *m >= 9 => {
                            Some(InvalidMatch::InvalidHttpMethod)
                        }
                        Some(http_types::http_method::Type::Registered(_)) => None,
                    }
                }
                Some(http::Match::Scheme(m)) => match m.type_.as_ref() {
                    None => Some(InvalidMatch::Empty),
                    Some(http_types::scheme::Type::Unregistered(_)) => None,
                    Some(http_types::scheme::Type::Registered(m)) if *m < 2 => None,
                    Some(http_types::scheme::Type::Registered(_)) => Some(InvalidMatch::InvalidScheme),
                }
                Some(http::Match::Authority(m)) => match m.match_.as_ref() {
                    None => Some(InvalidMatch::Empty),
                    Some(_) => None,
                }
                Some(http::Match::Path(m)) => match m.match_.as_ref() {
                    None => Some(InvalidMatch::Empty),
                    Some(_) => None,
                }
            };

            err == HttpMatch::try_from(http).err()
        }
    }
}

impl fmt::Display for InvalidMatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                InvalidMatch::Empty => "missing required field",
                InvalidMatch::InvalidPort => "invalid port number",
                InvalidMatch::InvalidNetwork => "invalid network address",
                InvalidMatch::InvalidHttpMethod => "invalid http method",
                InvalidMatch::InvalidScheme => "invalid request scheme",
            }
        )
    }
}

impl error::Error for InvalidMatch {}
