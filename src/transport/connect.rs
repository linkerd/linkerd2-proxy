extern crate tokio_connect;

pub use self::tokio_connect::Connect;

use std::io;
use std::net::SocketAddr;

use never::Never;
use svc;
use transport::{connection, tls};

#[derive(Debug, Clone)]
pub struct Stack {}

#[derive(Clone, Debug)]
pub struct Target {
    pub addr: SocketAddr,
    pub tls: tls::ConditionalConnectionConfig<tls::ClientConfig>,
    _p: (),
}

// ===== impl Target =====

impl Target {
    pub fn new(
        addr: SocketAddr,
        tls: tls::ConditionalConnectionConfig<tls::ClientConfig>
    ) -> Self {
        Self { addr, tls, _p: () }
    }

    pub fn tls_status(&self) -> tls::Status {
        self.tls.as_ref().map(|_| {})
    }
}

impl Connect for Target {
    type Connected = connection::Connection;
    type Error = io::Error;
    type Future = connection::Connecting;

    fn connect(&self) -> Self::Future {
        connection::connect(&self.addr, self.tls.clone())
    }
}

// ===== impl Stack =====

impl Stack {
    pub fn new() -> Self {
        Self {}
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
