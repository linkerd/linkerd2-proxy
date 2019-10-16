use super::tls::PeerIdentity;
use std::net::SocketAddr;

/// Describes an accepted connection.
#[derive(Clone, Debug)]
pub struct Source {
    pub remote: SocketAddr,
    pub local: SocketAddr,
    pub orig_dst: Option<SocketAddr>,
    pub tls_peer: PeerIdentity,
}

impl Source {
    pub fn orig_dst_if_not_local(&self) -> Option<SocketAddr> {
        self.orig_dst.and_then(|orig_dst| {
            // If the original destination is actually the listening socket,
            // we don't want to create a loop.
            if Self::same_addr(orig_dst, self.local) {
                None
            } else {
                Some(orig_dst)
            }
        })
    }

    fn same_addr(a0: SocketAddr, a1: SocketAddr) -> bool {
        use std::net::IpAddr::{V4, V6};
        (a0.port() == a1.port())
            && match (a0.ip(), a1.ip()) {
                (V6(a0), V4(a1)) => a0.to_ipv4() == Some(a1),
                (V4(a0), V6(a1)) => Some(a0) == a1.to_ipv4(),
                (a0, a1) => (a0 == a1),
            }
    }
}

// for logging context
impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.remote.fmt(f)
    }
}
