use crate::TcpEndpoint;
use linkerd_app_core::{
    svc::stack::{Either, Param, Predicate},
    transport::addrs::OrigDstAddr,
    Error,
};
use std::net::SocketAddr;
use thiserror::Error;

/// A connection policy that drops
#[derive(Copy, Clone, Debug)]
pub struct PreventLoop {
    port: u16,
}
#[derive(Copy, Clone, Debug)]
pub struct SwitchLoop {
    port: u16,
}

#[derive(Copy, Clone, Debug, Error)]
#[error("inbound connection must not target port {}", self.port)]
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
        let port = SocketAddr::from(t.target_addr).port();
        if port == self.port {
            Err(LoopPrevented { port }.into())
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

impl<T: Param<OrigDstAddr>> Predicate<T> for SwitchLoop {
    type Request = Either<T, T>;

    fn check(&mut self, addrs: T) -> Result<Either<T, T>, Error> {
        let OrigDstAddr(addr) = addrs.param();
        tracing::debug!(%addr, self.port);
        if addr.port() != self.port {
            Ok(Either::A(addrs))
        } else {
            Ok(Either::B(addrs))
        }
    }
}
