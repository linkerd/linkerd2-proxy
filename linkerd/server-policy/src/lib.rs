use futures::prelude::*;
use ipnet::IpNet;
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    time,
};

trait DiscoverServerPolicy {
    type Stream: Stream<Item = ServerPolicy> + Send + 'static;
    type Future: Future<Output = Self::Stream> + Send + 'static;

    fn discover(&self, port: u16) -> Self::Future;
}

#[derive(Clone, Debug)]
pub struct ServerPolicy {
    pub authorizations: Vec<Authz>,
    pub protocol: Protocol,
    pub labels: HashMap<String, String>,
}

#[derive(Copy, Clone, Debug)]
pub enum Protocol {
    Detect { timeout: time::Duration },
    Http1,
    Http2,
    Grpc,
    Opaque,
    Tls,
}

#[derive(Clone, Debug)]
pub struct Authz {
    pub networks: Vec<Network>,
    pub authn: Authn,
    labels: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Network {
    pub net: IpNet,
    pub except: Vec<IpNet>,
}

#[derive(Clone, Debug)]
pub enum Authn {
    Unauthenticated,
    TlsUnauthenticated,
    TlsAuthenticated {
        identities: HashSet<String>,
        suffixes: Vec<Suffix>,
    },
}

#[derive(Clone, Debug)]
pub struct Suffix {
    ends_with: String,
}

// === impl Network ===

impl Network {
    pub fn contains(&self, addr: &IpAddr) -> bool {
        self.net.contains(addr) && !self.except.iter().any(|net| net.contains(addr))
    }
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
