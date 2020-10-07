use ipnet::{Contains, IpNet};
use linkerd2_addr::Addr;
use linkerd2_dns::{Name, Suffix};
use std::{net::IpAddr, sync::Arc};

#[derive(Clone, Debug)]
pub struct AddrMatch {
    names: NameMatch,
    nets: IpMatch,
}

#[derive(Clone, Debug)]
pub struct NameMatch(Arc<Vec<Suffix>>);

#[derive(Clone, Debug)]
pub struct IpMatch(Arc<Vec<IpNet>>);

// === impl NameMatch ===

impl AddrMatch {
    pub fn new(
        suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self {
            names: NameMatch::new(suffixes),
            nets: IpMatch::new(nets),
        }
    }

    pub fn names(&self) -> &NameMatch {
        &self.names
    }

    pub fn nets(&self) -> &IpMatch {
        &self.nets
    }

    pub fn matches(&self, addr: &Addr) -> bool {
        match addr {
            Addr::Name(ref name) => self.names.matches(name.name()),
            Addr::Socket(sa) => self.nets.matches(sa.ip()),
        }
    }
}

// === impl NameMatch ===

impl NameMatch {
    pub fn new(suffixes: impl IntoIterator<Item = Suffix>) -> Self {
        Self(Arc::new(suffixes.into_iter().collect()))
    }

    pub fn matches(&self, name: &Name) -> bool {
        self.0.iter().any(|sfx| sfx.contains(name))
    }
}

// === impl IpMatch ===

impl IpMatch {
    pub fn new(nets: impl IntoIterator<Item = IpNet>) -> Self {
        Self(Arc::new(nets.into_iter().collect()))
    }

    pub fn matches(&self, addr: IpAddr) -> bool {
        self.0.iter().any(|net| match (net, addr) {
            (IpNet::V4(net), IpAddr::V4(ip)) => net.contains(&ip),
            (IpNet::V6(net), IpAddr::V6(ip)) => net.contains(&ip),
            _ => false,
        })
    }
}
