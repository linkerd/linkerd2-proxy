use crate::endpoint::{Accept, Concrete};
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Addr, Error, IpMatch};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

#[derive(Clone, Debug)]
pub struct AllowResolve;

impl FilterRequest<Accept> for AllowProfile {
    type Request = SocketAddr;

    fn filter(&self, Accept { orig_dst }: Accept) -> Result<SocketAddr, Error> {
        if self.0.matches(orig_dst.ip()) {
            Ok(orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

impl<P> FilterRequest<Concrete<P>> for AllowResolve {
    type Request = Addr;

    fn filter(&self, target: Concrete<P>) -> Result<Addr, Error> {
        target.resolve.ok_or_else(|| discovery_rejected().into())
    }
}
