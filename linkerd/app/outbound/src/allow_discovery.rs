use crate::target::Concrete;
use linkerd2_app_core::{discovery_rejected, svc::stack::FilterRequest, Addr, Error};

#[derive(Clone, Debug)]
pub struct AllowResolve;

impl<P> FilterRequest<Concrete<P>> for AllowResolve {
    type Request = Addr;

    fn filter(&self, target: Concrete<P>) -> Result<Addr, Error> {
        target.resolve.ok_or_else(|| discovery_rejected().into())
    }
}
