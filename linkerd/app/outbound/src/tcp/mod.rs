pub mod connect;
pub mod logical;
pub mod opaque_transport;
#[cfg(test)]
mod tests;

pub use self::connect::Connect;
use crate::target;
pub use linkerd_app_core::proxy::tcp::Forward;
use linkerd_app_core::{svc::Param, transport::OrigDstAddr, transport_header::SessionProtocol};

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
