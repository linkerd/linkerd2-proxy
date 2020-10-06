use ipnet::IpNet;
use linkerd2_app_core::{
    discovery_rejected, dns::Suffix, svc::stack::FilterRequest, Addr, AddrMatch, Error,
};

/// Rejects endpoint resolutions if the destination address is outside of the
/// configured networks/domains or if there is no resolvable concrete address.
#[derive(Clone, Debug)]
pub struct PermitResolve(AddrMatch);

impl PermitResolve {
    pub(super) fn new(
        suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self(AddrMatch::new(suffixes, nets))
    }
}

impl<T> FilterRequest<T> for PermitResolve
where
    for<'t> &'t T: Into<Option<Addr>>,
{
    type Request = Addr;

    fn filter(&self, t: T) -> Result<Addr, Error> {
        let permitted = (&t).into().and_then(|addr| {
            if self.0.matches(&addr) {
                Some(addr)
            } else {
                None
            }
        });
        tracing::debug!(permitted = %permitted.is_some(), "Resolve");
        permitted.ok_or_else(|| discovery_rejected().into())
    }
}
