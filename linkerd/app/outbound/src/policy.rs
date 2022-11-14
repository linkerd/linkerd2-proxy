use crate::Outbound;
use linkerd_app_core::{cache, transport::OrigDstAddr};
pub use linkerd_client_policy::*;
pub mod api;
mod discover;
pub use self::discover::Discover;
use linkerd_app_core::{dns, metrics, svc};

pub type Receiver = tokio::sync::watch::Receiver<ClientPolicy>;

#[derive(Clone, Debug)]
pub struct Policy {
    pub dst: OrigDstAddr,
    pub policy: cache::Cached<Receiver>,
}

impl Outbound<()> {
    pub fn build_policies(
        &self,
        dns: dns::Resolver,
        control_metrics: metrics::ControlHttp,
    ) -> impl svc::Service<OrigDstAddr, Response = Receiver> + Clone + Send + Sync + 'static {
        todo!("eliza: lol")
    }
}
