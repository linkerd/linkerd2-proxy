pub mod connect;
pub mod logical;
pub mod opaque_transport;
#[cfg(test)]
mod tests;

use crate::target;
pub use linkerd_app_core::proxy::tcp::Forward;
use linkerd_app_core::{
    svc::Param,
    transport::{listen, OrigDstAddr},
    transport_header::SessionProtocol,
};

pub type Accept = target::Accept<()>;
pub type Logical = target::Logical<()>;
pub type Concrete = target::Concrete<()>;
pub type Endpoint = target::Endpoint<()>;

impl From<OrigDstAddr> for Accept {
    fn from(orig_dst: OrigDstAddr) -> Self {
        Self {
            orig_dst,
            protocol: (),
        }
    }
}

impl std::convert::TryFrom<listen::Addrs> for Accept {
    type Error = std::io::Error;

    fn try_from(t: listen::Addrs) -> Result<Self, Self::Error> {
        match t.orig_dst() {
            Some(addr) => Ok(Self::from(addr)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No SO_ORIGINAL_DST address found",
            )),
        }
    }
}

impl<P> From<(P, Accept)> for target::Accept<P> {
    fn from((protocol, Accept { orig_dst, .. }): (P, Accept)) -> Self {
        Self { orig_dst, protocol }
    }
}

impl Param<Option<SessionProtocol>> for Endpoint {
    fn param(&self) -> Option<SessionProtocol> {
        None
    }
}
