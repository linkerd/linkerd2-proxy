use crate::target::Target;
use linkerd_app_core::{
    profiles::{DiscoveryRejected, LookupAddr},
    svc::stack::Predicate,
    Error, NameMatch,
};

#[derive(Clone, Debug)]
pub struct AllowProfile(pub NameMatch);

impl Predicate<Target> for AllowProfile {
    type Request = LookupAddr;

    fn check(&mut self, target: Target) -> Result<LookupAddr, Error> {
        let addr = target.dst.into_name_addr().ok_or_else(|| {
            DiscoveryRejected::new("inbound profile discovery requires DNS names")
        })?;
        if self.0.matches(addr.name()) {
            Ok(LookupAddr(addr.into()))
        } else {
            tracing::debug!(
                %addr,
                suffixes = %self.0,
                "Rejecting discovery, address not in configured DNS suffixes",
            );
            Err(DiscoveryRejected::new("address not in search DNS suffixes").into())
        }
    }
}
