use crate::spire;

pub use linkerd_app_core::identity::{client, Id};
use linkerd_app_core::{
    control, dns,
    identity::{
        client::linkerd::Certify, creds, CertMetrics, Credentials, DerX509, WithCertMetrics,
    },
    metrics::{prom, ControlHttp as ClientMetrics},
    Result,
};
use std::{future::Future, pin::Pin, time::SystemTime};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Linkerd {
        client: control::Config,
        certify: client::linkerd::Config,
        id: Id,
        server_name: dns::Name,
        trust_anchors_pem: String,
    },
    Spire {
        client: spire::Config,
        id: Id,
        server_name: dns::Name,
        trust_anchors_pem: String,
    },
}

// XXX(kate): keep a temporary tuple alias in place.
pub type TlsParams = (Id, dns::Name, String);

pub struct Identity {
    receiver: creds::Receiver,
    ready: watch::Receiver<bool>,
    task: Task,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Debug, Default)]
pub struct IdentityMetrics {
    cert: CertMetrics,
    client: control::Metrics,
}

/// Wraps a credential with a watch sender that notifies receivers when the store has been updated
/// at least once.
struct NotifyReady {
    store: creds::Store,
    tx: watch::Sender<bool>,
}

impl IdentityMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let cert = CertMetrics::register(registry.sub_registry_with_prefix("cert"));
        let client = control::Metrics::register(registry);
        Self { cert, client }
    }
}

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        client_metrics: ClientMetrics,
        metrics: IdentityMetrics,
    ) -> Result<Identity> {
        Ok(match self {
            Self::Linkerd {
                client,
                certify,
                id,
                server_name,
                trust_anchors_pem,
            } => {
                let certify = Certify::from(certify);
                let (store, receiver, ready) =
                    watch(id, server_name.clone(), trust_anchors_pem, metrics.cert)?;

                let task = {
                    let addr = client.addr.clone();
                    let svc =
                        client.build(dns, client_metrics, metrics.client, receiver.new_client());

                    Box::pin(certify.run(server_name, store, svc).instrument(
                        tracing::info_span!("identity", server.addr = %addr).or_current(),
                    ))
                };
                Identity {
                    receiver,
                    ready,
                    task,
                }
            }
            Self::Spire {
                client,
                id,
                server_name,
                trust_anchors_pem,
            } => {
                let addr = client.workload_api_addr.clone();
                let spire = spire::client::Spire::new(id.clone());

                let (store, receiver, ready) =
                    watch(id, server_name, trust_anchors_pem, metrics.cert)?;
                let task =
                    Box::pin(spire.run(store, spire::Client::from(client)).instrument(
                        tracing::info_span!("spire", server.addr = %addr).or_current(),
                    ));

                Identity {
                    receiver,
                    ready,
                    task,
                }
            }
        })
    }
}

fn watch(
    id: Id,
    server_name: dns::Name,
    trust_anchors_pem: String,
    metrics: CertMetrics,
) -> Result<(
    WithCertMetrics<NotifyReady>,
    creds::Receiver,
    watch::Receiver<bool>,
)> {
    let (tx, ready) = watch::channel(false);
    // XXX: this can simply accept the tls params.
    let (store, receiver) =
        linkerd_app_core::identity::creds::watch(id, server_name, &trust_anchors_pem)?;
    let cred = WithCertMetrics::new(metrics, NotifyReady { store, tx });
    Ok((cred, receiver, ready))
}

// === impl NotifyReady ===

impl Credentials for NotifyReady {
    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        key: Vec<u8>,
        exp: SystemTime,
    ) -> Result<()> {
        self.store.set_certificate(leaf, chain, key, exp)?;
        let _ = self.tx.send(true);
        Ok(())
    }
}

// === impl Identity ===

impl Identity {
    /// Returns a future that is satisfied once certificates have been provisioned.
    pub fn ready(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let mut ready = self.ready.clone();
        Box::pin(async move {
            while !*ready.borrow_and_update() {
                ready.changed().await.expect("identity sender must be held");
            }
        })
    }

    pub fn receiver(&self) -> creds::Receiver {
        self.receiver.clone()
    }

    pub fn run(self) -> Task {
        self.task
    }
}
