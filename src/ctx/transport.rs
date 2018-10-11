use indexmap::IndexMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use Conditional;
use control::destination;
use ctx;
use transport::tls;

#[derive(Debug)]
pub enum Ctx {
    Client(Arc<Client>),
    Server(Arc<Server>),
}

/// Identifies a connection from another process to a proxy listener.
#[derive(Debug)]
pub struct Server {
    pub proxy: ctx::Proxy,
    pub remote: SocketAddr,
    pub local: SocketAddr,
    pub orig_dst: Option<SocketAddr>,
    pub tls_status: tls::Status,
}

/// Identifies a connection from the proxy to another process.
#[derive(Debug)]
pub struct Client {
    pub proxy: ctx::Proxy,
    pub remote: SocketAddr,
    pub metadata: destination::Metadata,
    pub tls_status: tls::Status,
}

impl Ctx {
    pub fn proxy(&self) -> ctx::Proxy {
        match *self {
            Ctx::Client(ref ctx) => ctx.proxy,
            Ctx::Server(ref ctx) => ctx.proxy,
        }
    }

    pub fn tls_status(&self) -> tls::Status {
        match self {
            Ctx::Client(ctx) => ctx.tls_status,
            Ctx::Server(ctx) => ctx.tls_status,
        }
    }
}

impl Server {
    pub fn new(
        proxy: ctx::Proxy,
        local: &SocketAddr,
        remote: &SocketAddr,
        orig_dst: &Option<SocketAddr>,
        tls_status: tls::Status,
    ) -> Arc<Server> {
        let s = Server {
            proxy,
            local: *local,
            remote: *remote,
            orig_dst: *orig_dst,
            tls_status,
        };

        Arc::new(s)
    }

    pub fn orig_dst_if_not_local(&self) -> Option<SocketAddr> {
        match self.orig_dst {
            None => None,
            Some(orig_dst) => {
                // If the original destination is actually the listening socket,
                // we don't want to create a loop.
                if same_addr(&orig_dst, &self.local) {
                    None
                } else {
                    Some(orig_dst)
                }
            }
        }
    }
}

fn same_addr(a0: &SocketAddr, a1: &SocketAddr) -> bool {
    (a0.port() == a1.port()) && match (a0.ip(), a1.ip()) {
        (IpAddr::V6(a0), IpAddr::V4(a1)) => a0.to_ipv4() == Some(a1),
        (IpAddr::V4(a0), IpAddr::V6(a1)) => Some(a0) == a1.to_ipv4(),
        (a0, a1) => (a0 == a1),
    }
}

impl Client {
    pub fn new(
        proxy: ctx::Proxy,
        remote: &SocketAddr,
        metadata: destination::Metadata,
        tls_status: tls::Status,
    ) -> Arc<Client> {
        let c = Client {
            proxy,
            remote: *remote,
            metadata,
            tls_status,
        };

        Arc::new(c)
    }

    pub fn tls_identity(&self) -> Conditional<&tls::Identity, tls::ReasonForNoIdentity> {
        self.metadata.tls_identity()
    }

    pub fn labels(&self) -> &IndexMap<String, String> {
        self.metadata.labels()
    }
}

impl From<Arc<Client>> for Ctx {
    fn from(c: Arc<Client>) -> Self {
        Ctx::Client(c)
    }
}

impl From<Arc<Server>> for Ctx {
    fn from(s: Arc<Server>) -> Self {
        Ctx::Server(s)
    }
}

#[cfg(test)]
mod tests {
    use std::net;

    use quickcheck::TestResult;

    use super::same_addr;

    quickcheck! {
        fn same_addr_ipv4(ip0: net::Ipv4Addr, ip1: net::Ipv4Addr, port0: u16, port1: u16) -> TestResult {
            if port0 == 0 || port0 == ::std::u16::MAX {
                return TestResult::discard();
            } else if port1 == 0 || port1 == ::std::u16::MAX {
                return TestResult::discard();
            }

            let addr0 = net::SocketAddr::new(net::IpAddr::V4(ip0), port0);
            let addr1 = net::SocketAddr::new(net::IpAddr::V4(ip1), port1);
            TestResult::from_bool(same_addr(&addr0, &addr1) == (addr0 == addr1))
        }

        fn same_addr_ipv6(ip0: net::Ipv6Addr, ip1: net::Ipv6Addr, port0: u16, port1: u16) -> TestResult {
            if port0 == 0 || port0 == ::std::u16::MAX {
                return TestResult::discard();
            } else if port1 == 0 || port1 == ::std::u16::MAX {
                return TestResult::discard();
            }

            let addr0 = net::SocketAddr::new(net::IpAddr::V6(ip0), port0);
            let addr1 = net::SocketAddr::new(net::IpAddr::V6(ip1), port1);
            TestResult::from_bool(same_addr(&addr0, &addr1) == (addr0 == addr1))
        }

        fn same_addr_ip6_mapped_ipv4(ip: net::Ipv4Addr, port: u16) -> TestResult {
            if port == 0 || port == ::std::u16::MAX {
                return TestResult::discard();
            }

            let addr4 = net::SocketAddr::new(net::IpAddr::V4(ip), port);
            let addr6 = net::SocketAddr::new(net::IpAddr::V6(ip.to_ipv6_mapped()), port);
            TestResult::from_bool(same_addr(&addr4, &addr6))
        }

        fn same_addr_ip6_compat_ipv4(ip: net::Ipv4Addr, port: u16) -> TestResult {
            if port == 0 || port == ::std::u16::MAX {
                return TestResult::discard();
            }

            let addr4 = net::SocketAddr::new(net::IpAddr::V4(ip), port);
            let addr6 = net::SocketAddr::new(net::IpAddr::V6(ip.to_ipv6_compatible()), port);
            TestResult::from_bool(same_addr(&addr4, &addr6))
        }
    }
}
