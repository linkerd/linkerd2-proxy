use linkerd2_app_core::{admit, transport::connect};

/// A connection policy that drops
#[derive(Copy, Clone, Debug)]
pub struct PreventLoop {
    port: u16,
}

#[derive(Copy, Clone, Debug)]
pub struct LoopPrevented {
    port: u16,
}

impl From<u16> for PreventLoop {
    fn from(port: u16) -> Self {
        Self { port }
    }
}

impl<T: connect::ConnectAddr> admit::Admit<T> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &T) -> Result<(), Self::Error> {
        let addr = ep.connect_addr();
        tracing::debug!(%addr, self.port);
        if addr.ip().is_loopback() && addr.port() == self.port {
            return Err(LoopPrevented { port: self.port });
        }
        Ok(())
    }
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "outbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
