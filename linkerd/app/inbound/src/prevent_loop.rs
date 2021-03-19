use crate::TcpEndpoint;
use linkerd_app_core::{
    svc::stack::{Either, Param, Predicate},
    transport::addrs::OrigDstAddr,
    Error,
};

/// A connection policy that drops
#[derive(Copy, Clone, Debug)]
pub struct PreventLoop {
    port: u16,
}
#[derive(Copy, Clone, Debug)]
pub struct SwitchLoop {
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

impl PreventLoop {
    pub fn to_switch(self) -> SwitchLoop {
        SwitchLoop { port: self.port }
    }
}

impl<T: Param<Option<OrigDstAddr>>> Predicate<T> for SwitchLoop {
    type Request = Either<T, T>;

    fn check(&mut self, addrs: T) -> Result<Either<T, T>, Error> {
        let OrigDstAddr(addr) = addrs.param().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No SO_ORIGINAL_DST address found",
            )
        })?;
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
        write!(f, "inbound connection must not target port {}", self.port)
    }
}

impl std::error::Error for LoopPrevented {}
