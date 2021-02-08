use crate::target::Target;
use linkerd_app_core::{
    discovery_rejected, profiles::LogicalAddr, svc::stack::Predicate, Error, NameMatch,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub NameMatch);

impl Predicate<Target> for AllowProfile {
    type Request = LogicalAddr;

    fn check(&mut self, target: Target) -> Result<LogicalAddr, Error> {
        let addr = target.dst.into_name_addr().ok_or_else(discovery_rejected)?;
        if self.0.matches(addr.name()) {
            Ok(LogicalAddr(addr.into()))
        } else {
            Err(discovery_rejected().into())
        }
    }
}
