pub use self::{config::Config, discover::Discover};
use crate::Outbound;
use linkerd_app_core::{cache, dns, metrics, svc, transport::OrigDstAddr, Error};
pub use linkerd_client_policy::*;
pub mod api;
mod config;
mod discover;
pub mod http;

pub type Receiver = tokio::sync::watch::Receiver<ClientPolicy>;

#[derive(Clone, Debug)]
pub struct Policy {
    pub policy: cache::Cached<Receiver>,
}

// === impl Policy ===

impl Policy {
    pub fn backends(&self) -> Vec<split::Backend> {
        self.policy.borrow().backends.clone()
    }

    pub fn backend_stream(&self) -> split::BackendStream {
        let mut rx = self.policy.clone();
        let stream = async_stream::stream! {
            while rx.changed().await.is_ok() {
                let backends = rx.borrow_and_update().backends.clone();
                yield backends;
            }
        };
        split::BackendStream(Box::pin(stream))
    }
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
