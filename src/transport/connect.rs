extern crate tokio_connect;

pub use self::tokio_connect::Connect;

use std::{hash, io, net::SocketAddr};

use never::Never;
use svc;
use Conditional;

#[derive(Debug, Clone)]
pub struct Stack(())

/// A TCP connection target, optionally with TLS.
#[derive(Clone, Debug, Hash, PartialEq, Eq )]
pub struct Target(SocketAddr)

// ===== impl Target =====

impl From<SocketAddr> for Target {
    fn from(addr: SocketAddr) -> Self {
        Target(addr)
    }
}

impl Into<SocketAddr> for Target {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl Connect for Target {
    type Connected = connection::Connection;
    type Error = io::Error;
    type Future = connection::Connecting;

    fn connect(&self) -> Self::Future {
        connection::connect(&self.addr)
    }
}

// ===== impl Stack =====

impl Stack {
    pub fn new() -> Self {
        Stack(())
    }
}

impl<T> svc::Stack<T> for Stack
where
    T: Clone,
    Target: From<T>,
{
    type Value = Target;
    type Error = Never;

    fn make(&self, t: &T) -> Result<Self::Value, Self::Error> {
        Ok(t.clone().into())
    }
}
