use crate::svc::Predicate;
pub use linkerd_proxy_transport::*;

pub mod labels;

pub type Metrics = metrics::Registry<labels::Key>;

#[derive(Copy, Clone, Debug)]
pub struct AddrsFilter<G>(pub G);

impl<'a, G, T> Predicate<&'a T> for AddrsFilter<G>
where
    G: GetAddrs<T>,
{
    type Request = G::Addrs;

    fn check(&mut self, target: &'a T) -> Result<Self::Request, crate::Error> {
        self.0.addrs(target).map_err(Into::into)
    }
}
