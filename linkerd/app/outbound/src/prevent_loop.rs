use super::endpoint::{HttpEndpoint, Target};
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

impl admit::Admit<Target<HttpEndpoint>> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &Target<HttpEndpoint>) -> Result<(), Self::Error> {
        tracing::debug!(addr = %ep.inner.addr, self.port);
        if ep.inner.addr.ip().is_loopback() && ep.inner.addr.port() == self.port {
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
