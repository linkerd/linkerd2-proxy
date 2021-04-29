use crate::target::Target;
use linkerd_app_core::{
    profiles::LookupAddr, svc::stack::Predicate, DiscoveryRejected, Error, NameMatch,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub NameMatch);

impl Predicate<Target> for AllowProfile {
    type Request = LookupAddr;

    fn check(&mut self, target: Target) -> Result<LookupAddr, Error> {
        let addr = target.dst.into_name_addr().ok_or_else(|| {
            DiscoveryRejected::message("inbound profile discovery requires DNS names")
        })?;
        if self.0.matches(addr.name()) {
            Ok(LookupAddr(addr.into()))
        } else {
            Err(DiscoveryRejected::from(self.0.clone()).into())
        }
    }
}
