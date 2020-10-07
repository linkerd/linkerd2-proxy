use crate::endpoint::{HttpConcrete, HttpLogical, TcpLogical};
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Addr, AddrMatch, Error};
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AllowProfile(pub AddrMatch);

#[derive(Clone, Debug)]
pub struct AllowTcpResolve(pub AddrMatch);

#[derive(Copy, Clone, Debug)]
pub struct AllowHttpResolve;

impl FilterRequest<HttpLogical> for AllowProfile {
    type Request = Addr;

    fn filter(&self, target: HttpLogical) -> Result<Addr, Error> {
        if self.0.matches(&target.dst) {
            Ok(target.dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

impl FilterRequest<HttpConcrete> for AllowHttpResolve {
    type Request = Addr;

    fn filter(&self, target: HttpConcrete) -> Result<Addr, Error> {
        target.resolve.ok_or_else(|| discovery_rejected().into())
    }
}

impl FilterRequest<TcpLogical> for AllowTcpResolve {
    type Request = SocketAddr;

    fn filter(&self, target: TcpLogical) -> Result<SocketAddr, Error> {
        if self.0.nets().matches(target.addr.ip()) {
            Ok(target.addr)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
