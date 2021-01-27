use crate::TcpEndpoint;
use linkerd_app_core::{
    svc::stack::{Either, Predicate},
    transport::listen::Addrs,
    Error,
};

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

impl Predicate<TcpEndpoint> for PreventLoop {
    type Request = TcpEndpoint;

    fn check(&mut self, t: TcpEndpoint) -> Result<TcpEndpoint, Error> {
        if t.port == self.port {
            Err(LoopPrevented { port: t.port }.into())
        } else {
            Ok(t)
        }
    }
}

impl Predicate<Addrs> for PreventLoop {
    type Request = Either<Addrs, Addrs>;

    fn check(&mut self, addrs: Addrs) -> Result<Either<Addrs, Addrs>, Error> {
        let addr = addrs.target_addr();
        tracing::debug!(%addr, self.port);
        if addr.port() != self.port {
            Ok(Either::A(addrs))
        } else {
            Ok(Either::B(addrs))
        }
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
