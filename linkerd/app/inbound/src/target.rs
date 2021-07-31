use linkerd_app_core::{svc, transport};

// #[derive(Clone, Debug, PartialEq, Eq, Hash)]
// pub struct HttpAccept {
//     pub tcp: TcpAccept,
//     pub version: http::Version,
// }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub port: u16,
}

// === impl TcpAccept ===

/*
impl TcpAccept {
    pub fn port_skipped<T>(tcp: T) -> Self
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
    {
        let OrigDstAddr(target_addr) = tcp.param();
        Self {
            target_addr,
            client_addr: tcp.param(),
            tls: Conditional::None(tls::NoServerTls::PortSkipped),
        }
    }
}

impl<T> From<(tls::ConditionalServerTls, T)> for TcpAccept
where
    T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
{
    fn from((tls, addrs): (tls::ConditionalServerTls, T)) -> Self {
        let OrigDstAddr(target_addr) = addrs.param();
        Self {
            target_addr,
            client_addr: addrs.param(),
            tls,
        }
    }
}

impl Param<SocketAddr> for TcpAccept {
    fn param(&self) -> SocketAddr {
        self.target_addr
    }
}

impl Param<OrigDstAddr> for TcpAccept {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(self.target_addr)
    }
}

impl Param<transport::labels::Key> for TcpAccept {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::accept(
            transport::labels::Direction::In,
            self.tls.clone(),
            self.target_addr,
        )
    }
}

// === impl HttpAccept ===

impl From<(http::Version, TcpAccept)> for HttpAccept {
    fn from((version, tcp): (http::Version, TcpAccept)) -> Self {
        Self { version, tcp }
    }
}

impl Param<http::Version> for HttpAccept {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<http::normalize_uri::DefaultAuthority> for HttpAccept {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(
            http::uri::Authority::from_str(&self.tcp.target_addr.to_string())
                .expect("Address must be a valid authority"),
        ))
    }
}

impl Param<Option<identity::Name>> for HttpAccept {
    fn param(&self) -> Option<identity::Name> {
        self.tcp
            .tls
            .value()
            .and_then(|server_tls| match server_tls {
                tls::ServerTls::Established {
                    client_id: Some(id),
                    ..
                } => Some(id.clone().0),
                _ => None,
            })
    }
}
 */

// === TcpEndpoint ===

impl TcpEndpoint {
    pub fn from_param<T: svc::Param<u16>>(t: T) -> Self {
        Self { port: t.param() }
    }
}

impl svc::Param<u16> for TcpEndpoint {
    fn param(&self) -> u16 {
        self.port
    }
}

impl svc::Param<transport::labels::Key> for TcpEndpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}

// Needed by `linkerd_app_test::Connect`
#[cfg(test)]
impl From<TcpEndpoint> for SocketAddr {
    fn from(ep: TcpEndpoint) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], ep.port))
    }
}
