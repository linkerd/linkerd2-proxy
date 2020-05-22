use super::endpoint::{Target, TcpEndpoint};
use linkerd2_app_core::admit;

/// A connection policy that drops
#[derive(Copy, Clone, Debug)]
pub struct PreventLoop {
    port: u16,
}

#[derive(Copy, Clone, Debug)]
pub struct LoopPrevented {
    port: u16,
}

impl PreventLoop {
    pub fn new(port: u16) -> Self {
        Self { port }
    }
}

impl admit::Admit<Target> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &Target) -> Result<(), Self::Error> {
        tracing::debug!(addr = %ep.addr, self.port);
        if ep.addr.port() == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
    }
}

impl admit::Admit<TcpEndpoint> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &TcpEndpoint) -> Result<(), Self::Error> {
        tracing::debug!(port = %ep.port, self.port);
        if ep.port == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
    }
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "inbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
