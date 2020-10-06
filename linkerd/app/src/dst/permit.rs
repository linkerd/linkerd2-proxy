use super::Rejected;
use ipnet::IpNet;
use linkerd2_app_core::{
    addr_match::AddrMatch, dns::Suffix, svc::stack::FilterRequest, Addr, Error,
};

/// Rejects profile lookups if the destination address is outside of the
/// configured networks/domains.
#[derive(Clone, Debug)]
pub struct PermitProfile(AddrMatch);

/// Rejects endpoint resolutions if the destinatino address is outside of the
/// configured networks/domains or if there is no resolvable concrete address.
#[derive(Clone, Debug)]
pub struct PermitResolve(AddrMatch);

// === impl PermitProfile ===

impl PermitProfile {
    pub(super) fn new(
        suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self(AddrMatch::new(suffixes, nets))
    }
}

impl<T> FilterRequest<T> for PermitProfile
where
    for<'t> &'t T: Into<Addr>,
{
    type Request = Addr;

    fn filter(&self, t: T) -> Result<Addr, Error> {
        let addr = (&t).into();
        let permitted = self.0.matches(&addr);
        tracing::debug!(permitted, "Profile");
        if permitted {
            Ok(addr)
        } else {
            Err(Rejected(()).into())
        }
    }
}

// === impl PermitResolve ===

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
        permitted.ok_or_else(|| Rejected(()).into())
    }
}
