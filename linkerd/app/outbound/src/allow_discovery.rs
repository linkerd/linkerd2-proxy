use crate::endpoint::TcpAccept;
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Error, IpMatch};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

impl FilterRequest<TcpAccept> for AllowProfile {
    type Request = SocketAddr;

    fn filter(&self, target: TcpAccept) -> Result<SocketAddr, Error> {
        if self.0.matches(target.addr.ip()) {
            Ok(target.addr)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
