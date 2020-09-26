use super::Rejected;
use ipnet::{Contains, IpNet};
use linkerd2_app_core::{dns::Suffix, request_filter, Addr, Error};
use std::net::IpAddr;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PermitConfiguredDsts {
    name_suffixes: Arc<Vec<Suffix>>,
    networks: Arc<Vec<IpNet>>,
}

// === impl PermitConfiguredDsts ===

impl PermitConfiguredDsts {
    pub fn new(
        name_suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self {
            name_suffixes: Arc::new(name_suffixes.into_iter().collect()),
            networks: Arc::new(nets.into_iter().collect()),
        }
    }
}

impl<T> request_filter::RequestFilter<T> for PermitConfiguredDsts
where
    for<'t> &'t T: Into<Addr>,
{
    type Error = Error;

    fn filter(&self, t: T) -> Result<T, Self::Error> {
        let permitted = match (&t).into() {
            Addr::Name(ref name) => self
                .name_suffixes
                .iter()
                .any(|suffix| suffix.contains(name.name())),
            Addr::Socket(sa) => self.networks.iter().any(|net| match (net, sa.ip()) {
                (IpNet::V4(net), IpAddr::V4(addr)) => net.contains(&addr),
                (IpNet::V6(net), IpAddr::V6(addr)) => net.contains(&addr),
                _ => false,
            }),
        };

        if permitted {
            Ok(t)
        } else {
            Err(Rejected(()).into())
        }
    }
}
