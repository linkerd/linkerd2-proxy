use crate::endpoint::Target;
use linkerd2_app_core::{
    discovery_rejected, svc::stack::FilterRequest, Error, NameAddr, NameMatch,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub NameMatch);

impl FilterRequest<Target> for AllowProfile {
    type Request = NameAddr;

    fn filter(&self, target: Target) -> Result<NameAddr, Error> {
        let addr = target
            .dst
            .into_name_addr()
            .ok_or_else(|| discovery_rejected())?;
        if self.0.matches(addr.name()) {
            Ok(addr)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
