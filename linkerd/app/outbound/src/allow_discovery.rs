use crate::endpoint::HttpLogical;
use linkerd2_app_core::{
    addr_match::AddrMatch, discovery_rejected, svc::stack::FilterRequest, Addr, Error,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub AddrMatch);

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
