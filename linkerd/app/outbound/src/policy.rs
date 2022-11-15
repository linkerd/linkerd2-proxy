pub use self::{config::Config, discover::Discover};
use crate::Outbound;
use linkerd_app_core::{cache, dns, metrics, svc, transport::OrigDstAddr, Error};
pub use linkerd_client_policy::*;
pub mod api;
mod config;
mod discover;

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
        metrics: metrics::ControlHttp,
    ) -> impl svc::Service<OrigDstAddr, Response = Receiver, Future = impl Send, Error = Error>
           + Clone
           + Send
           + Sync
           + 'static {
        self.config
            .policy
            .clone()
            .build(dns, metrics, self.runtime.identity.clone())
    }
}
