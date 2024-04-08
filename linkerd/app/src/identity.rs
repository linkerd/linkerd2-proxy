pub mod linkerd_config;
pub mod spire_config;

pub use linkerd_app_core::identity::{client, Id};

use linkerd_app_core::{
    dns,
    identity::{creds, CertMetrics},
    metrics::{prom, ControlHttp as ClientMetrics},
    Result,
};
use std::{future::Future, pin::Pin};
use tokio::sync::watch;
use tokio::time;
use tracing::{info_span, Instrument};

#[derive(Debug, thiserror::Error)]
#[error("linkerd identity requires a TLS Id and server name to be the same")]
pub struct TlsIdAndServerNameNotMatching(());

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Linkerd(linkerd_config::Config),
    Spire(spire_config::Config),
}

#[derive(Clone, Debug)]
pub struct TlsParams {
    pub id: Id,
    pub server_name: dns::Name,
    pub trust_anchors_pem: String,
}

pub struct Identity {
    ready: watch::Receiver<Option<creds::Receiver>>,
    task: Task,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// === impl Config ===

impl Config {
    pub fn try_from_env() -> Result<Self, super::env::EnvError> {
        super::env::Env.try_config().map(|c| c.identity)
    }

    pub fn build(
        self,
        dns: dns::Resolver,
        client_metrics: ClientMetrics,
        registry: &mut prom::Registry,
    ) -> Result<Identity> {
        let cert_metrics =
            CertMetrics::register(registry.sub_registry_with_prefix("identity_cert"));
        match self {
            Self::Linkerd(linkerd) => linkerd.build(cert_metrics, dns, client_metrics, registry),
            Self::Spire(spire) => spire.build(cert_metrics),
        }
    }
}

// === impl Identity ===

impl Identity {
    pub async fn initialize(self) -> creds::Receiver {
        // spawn the identity task
        tokio::spawn(self.task.instrument(info_span!("identity").or_current()));

        let mut ready = self.ready.clone();
        const TIMEOUT: time::Duration = time::Duration::from_secs(15);

        let mut receiver_fut = Box::pin(async move {
            loop {
                if let Some(receiver) = (*ready.borrow_and_update()).clone() {
                    return receiver;
                }
                ready.changed().await.expect("identity sender must be held");
            }
        });

        loop {
            tokio::select! {
                receiver = (&mut receiver_fut) => return receiver,
                _ = time::sleep(TIMEOUT) => {
                    tracing::warn!("Waiting for identity to be initialized...");
                }
            }
        }
    }
}
