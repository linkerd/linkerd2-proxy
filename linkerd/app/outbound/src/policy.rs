pub use self::{config::Config, discover::Discover};
use crate::Outbound;
use linkerd_app_core::{
    dns, metrics,
    svc::{self, ServiceExt},
    transport::OrigDstAddr,
    Error,
};
pub use linkerd_client_policy::*;
use tokio::sync::watch;

pub mod api;
mod config;
mod discover;

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
        match self.config.policy {
            Some(ref config) => svc::Either::A(
                config
                    .clone()
                    .build(dns, metrics, self.runtime.identity.clone())
                    .map_response(Into::into),
            ),
            None => {
                tracing::info!("No policy service configured, using default client policy.");

                static DEFAULT_POLICY: once_cell::sync::Lazy<(
                    watch::Sender<Policy>,
                    watch::Receiver<Policy>,
                )> = once_cell::sync::Lazy::new(|| tokio::sync::watch::channel(Policy::default()));

                svc::Either::B(svc::mk(|_| {
                    futures::future::ready::<Result<_, Error>>(Ok(DEFAULT_POLICY.1.clone().into()))
                }))
            }
        }
    }
}
