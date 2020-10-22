pub mod connect;
#[cfg(test)]
mod tests;

use crate::target;
pub use linkerd2_app_core::proxy::tcp::*;
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Error, IpMatch};

pub type Accept = target::Accept<()>;
pub type Logical = target::Logical<()>;
pub type Concrete = target::Concrete<()>;
pub type Endpoint = target::Endpoint<()>;

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl FilterRequest<Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn filter(&self, Accept { orig_dst, .. }: Accept) -> Result<std::net::SocketAddr, Error> {
        if self.0.matches(orig_dst.ip()) {
            Ok(orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
