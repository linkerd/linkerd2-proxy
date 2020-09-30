use ipnet::{Contains, IpNet};
use linkerd2_app_core::{
    dns::Suffix, request_filter::FilterRequest, Addr, DiscoveryRejected, Error,
};
use std::marker::PhantomData;
use std::net::IpAddr;
use std::sync::Arc;

pub struct PermitConfiguredDsts<E = DiscoveryRejected> {
    name_suffixes: Arc<Vec<Suffix>>,
    networks: Arc<Vec<IpNet>>,
    _error: PhantomData<fn(E)>,
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
            _error: PhantomData,
        }
    }

    /// Configures the returned error type when the target is outside of the
    /// configured set of destinations.
    pub fn with_error<E>(self) -> PermitConfiguredDsts<E>
    where
        E: Into<Error> + From<Addr>,
    {
        PermitConfiguredDsts {
            name_suffixes: self.name_suffixes,
            networks: self.networks,
            _error: PhantomData,
        }
    }
}

impl<E> Clone for PermitConfiguredDsts<E> {
    fn clone(&self) -> Self {
        Self {
            name_suffixes: self.name_suffixes.clone(),
            networks: self.networks.clone(),
            _error: PhantomData,
        }
    }
}

impl<T, E> FilterRequest<T> for PermitConfiguredDsts<E>
where
    E: Into<Error> + From<Addr>,
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

        if permitted {
            Ok(t)
        } else {
            Err(E::from(addr.clone()).into())
        }
    }
}
