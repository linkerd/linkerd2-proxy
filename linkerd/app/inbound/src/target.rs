use linkerd_app_core::{svc, transport};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub port: u16,
}

// === impl TcpAccept ===

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
