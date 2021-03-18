use crate::target::Target;
use linkerd_app_core::{
    discovery_rejected, profiles::LookupAddr, svc::stack::Predicate, Error, NameMatch,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub NameMatch);

impl Predicate<Target> for AllowProfile {
    type Request = LookupAddr;

    fn check(&mut self, target: Target) -> Result<LookupAddr, Error> {
        let addr = target.dst.into_name_addr().ok_or_else(discovery_rejected)?;
        if self.0.matches(addr.name()) {
            Ok(LookupAddr(addr.into()))
        } else {
            Err(discovery_rejected().into())
        }
    }
}
