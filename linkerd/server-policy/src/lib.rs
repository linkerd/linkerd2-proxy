#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

mod network;

pub use self::network::Network;
use std::{collections::HashSet, hash::Hash, sync::Arc, time};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPolicy {
    pub protocol: Protocol,
    pub authorizations: Vec<Authorization>,
    pub name: Arc<str>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Detect { timeout: time::Duration },
    Http1,
    Http2,
    Grpc,
    Opaque,
    Tls,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    pub networks: Vec<Network>,
    pub authentication: Authentication,
    pub name: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authentication {
    Unauthenticated,
    TlsUnauthenticated,
    TlsAuthenticated {
        identities: HashSet<String>,
        suffixes: Vec<Suffix>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suffix {
    ends_with: String,
}

// === impl Suffix ===

impl From<Vec<String>> for Suffix {
    fn from(parts: Vec<String>) -> Self {
        let ends_with = if parts.is_empty() {
            "".to_string()
        } else {
            format!(".{}", parts.join("."))
        };
        Suffix { ends_with }
    }
}

impl Suffix {
    #[inline]
    pub fn contains(&self, name: &str) -> bool {
        name.ends_with(&self.ends_with)
    }
}

#[cfg(test)]
mod network_tests {
    use super::Network;
    use ipnet::{IpNet, Ipv4Net, Ipv6Net};
    use quickcheck::{quickcheck, TestResult};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    quickcheck! {
        fn contains_v4(addr: Ipv4Addr, exclude: Option<Ipv4Addr>) -> TestResult {
            let net = Network {
                net: Ipv4Net::default().into(),
                except: exclude.into_iter().map(|a| IpNet::from(IpAddr::V4(a))).collect(),
            };

            if let Some(e) = exclude {
                if net.contains(&e.into()) {
                    return TestResult::failed();
                }
                if addr == e {
                    return TestResult::passed();
                }
            }
            TestResult::from_bool(net.contains(&addr.into()))
        }

        fn contains_v6(addr: Ipv6Addr, exclude: Option<Ipv6Addr>) -> TestResult {
            let net = Network {
                net: Ipv6Net::default().into(),
                except: exclude.into_iter().map(|a| IpNet::from(IpAddr::V6(a))).collect(),
            };

            if let Some(e) = exclude {
                if net.contains(&e.into()) {
                    return TestResult::failed();
                }
                if addr == e {
                    return TestResult::passed();
                }
            }
            TestResult::from_bool(net.contains(&addr.into()))
        }
    }
}
