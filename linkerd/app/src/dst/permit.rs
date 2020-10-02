use super::Rejected;
use ipnet::{Contains, IpNet};
use linkerd2_app_core::{dns::Suffix, request_filter::FilterRequest, Addr, Error};
use std::{net::IpAddr, sync::Arc};

#[derive(Clone, Debug)]
pub struct PermitProfile(Inner);

#[derive(Clone, Debug)]
pub struct PermitResolve(Inner);

#[derive(Clone, Debug)]
struct Inner {
    names: Arc<Vec<Suffix>>,
    networks: Arc<Vec<IpNet>>,
}

// === impl PermitProfile ===

impl PermitProfile {
    pub(super) fn new(
        name_suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self(Inner {
            names: Arc::new(name_suffixes.into_iter().collect()),
            networks: Arc::new(nets.into_iter().collect()),
        })
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
        tracing::debug!(%addr, permitted, "Profile");
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
        name_suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self(Inner {
            names: Arc::new(name_suffixes.into_iter().collect()),
            networks: Arc::new(nets.into_iter().collect()),
        })
    }
}

impl<T> FilterRequest<T> for PermitResolve
where
    for<'t> &'t T: Into<Option<Addr>>,
{
    type Request = Addr;

    fn filter(&self, t: T) -> Result<Addr, Error> {
        let addr = match (&t).into() {
            Some(addr) => addr,
            None => {
                tracing::debug!("No resolveable address");
                return Err(Rejected(()).into());
            }
        };
        let permitted = self.0.matches(&addr);
        tracing::debug!(%addr, permitted, "Resolve");
        if permitted {
            Ok(addr)
        } else {
            Err(Rejected(()).into())
        }
    }
}

// === impl Inner ===

impl Inner {
    fn matches(&self, addr: &Addr) -> bool {
        match addr {
            Addr::Name(ref name) => self.names.iter().any(|sfx| sfx.contains(name.name())),
            Addr::Socket(sa) => self.networks.iter().any(|net| match (net, sa.ip()) {
                (IpNet::V4(net), IpAddr::V4(addr)) => net.contains(&addr),
                (IpNet::V6(net), IpAddr::V6(addr)) => net.contains(&addr),
                _ => false,
            }),
        }
    }
}
