use super::Rejected;
use ipnet::{Contains, IpNet};
use linkerd2_app_core::{
    dns::Suffix, request_filter::FilterRequest, Addr, DiscoveryRejected, Error,
};
use std::{net::IpAddr, sync::Arc};

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

impl<T, E> FilterRequest<T> for PermitConfiguredDsts<E>
where
    for<'t> &'t T: Into<Addr>,
{
    type Request = T;

    fn filter(&self, t: T) -> Result<T, Error> {
        let addr = (&t).into();
        let permitted = match addr {
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

        tracing::debug!(%addr, permitted);
        if permitted {
            Ok(t)
        } else {
            Err(Rejected(()).into())
        }
    }
}
