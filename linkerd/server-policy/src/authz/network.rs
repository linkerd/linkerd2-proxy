use ipnet::IpNet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    pub net: IpNet,
    pub except: Vec<IpNet>,
}

// === impl Network ===

impl From<IpNet> for Network {
    fn from(net: IpNet) -> Self {
        Self {
            net,
            except: vec![],
        }
    }
}

impl From<ipnet::Ipv4Net> for Network {
    fn from(net: ipnet::Ipv4Net) -> Self {
        Self::from(IpNet::from(net))
    }
}

impl From<ipnet::Ipv6Net> for Network {
    fn from(net: ipnet::Ipv6Net) -> Self {
        Self::from(IpNet::from(net))
    }
}

impl From<std::net::IpAddr> for Network {
    fn from(addr: std::net::IpAddr) -> Self {
        Self::from(ipnet::IpNet::from(addr))
    }
}

impl From<std::net::Ipv4Addr> for Network {
    fn from(addr: std::net::Ipv4Addr) -> Self {
        Self::from(ipnet::Ipv4Net::from(addr))
    }
}

impl From<std::net::Ipv6Addr> for Network {
    fn from(addr: std::net::Ipv6Addr) -> Self {
        Self::from(ipnet::Ipv6Net::from(addr))
    }
}

impl Network {
    #[inline]
    pub fn contains(&self, ip: &std::net::IpAddr) -> bool {
        self.net.contains(ip) && !self.except.iter().any(|net| net.contains(ip))
    }
}

impl std::str::FromStr for Network {
    type Err = ipnet::AddrParseError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self {
            net: s.parse()?,
            except: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
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
