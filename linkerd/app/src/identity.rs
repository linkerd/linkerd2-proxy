use crate::spire;
pub use linkerd_app_core::identity::{
    client::{certify, TokenSource},
    spire_client::Spire,
    Id,
};
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity::{
        client::Certify, client_metrics::Metrics as IdentityMetrics, creds, Credentials, DerX509,
        Mode,
    },
    metrics::{prom, ControlHttp as ClientMetrics},
    Error, Result,
};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Provider {
    ControlPlane {
        control: control::Config,
        certify: certify::Config,
    },
    Spire(spire::Config),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub params: TlsParams,
}

#[derive(Clone, Debug)]
pub struct TlsParams {
    pub server_id: Id,
    pub server_name: dns::Name,
    pub trust_anchors_pem: String,
}

#[derive(Clone)]
pub enum Addr {
    Spire(Arc<String>),
    Linkerd(control::ControlAddr),
}

pub struct Identity {
    addr: Addr,
    receiver: creds::Receiver,
    ready: watch::Receiver<bool>,
    metrics: IdentityMetrics,
    task: Task,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

/// Wraps a credential with a watch sender that notifies receivers when the store has been updated
/// at least once.
struct NotifyReady {
    store: creds::Store,
    tx: watch::Sender<bool>,
}

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        client_metrics: ClientMetrics,
        registry: &mut prom::Registry,
    ) -> Result<Identity> {
        let name = self.params.server_name.clone();
        let (store, receiver) = Mode::default().watch(
            name.clone().into(),
            name.clone(),
            &self.params.trust_anchors_pem,
        )?;

        let metrics = IdentityMetrics::default();
        let (tx, ready) = watch::channel(false);
        let cred = NotifyReady { store, tx };

        let identity = match self.provider {
            Provider::ControlPlane { control, certify } => {
                let certify = Certify::new(certify, metrics.clone());
                let addr = control.addr.clone();

                let task = Box::pin({
                    let addr = addr.clone();
                    let svc = control.build(dns, client_metrics, registry, receiver.new_client());

                    certify.run(name, cred, svc).instrument(
                        tracing::debug_span!("identity", server.addr = %addr).or_current(),
                    )
                });
                Identity {
                    addr: Addr::Linkerd(addr),
                    receiver,
                    metrics,
                    ready,
                    task,
                }
            }
            Provider::Spire(cfg) => {
                let addr = cfg.socket_addr.clone();
                let spire = Spire::new(self.params.server_id.clone(), metrics.clone());
                let task = Box::pin({
                    let client = spire::Client::from(cfg);
                    spire.run(cred, client).instrument(
                        tracing::debug_span!("spire identity", server.addr = %addr).or_current(),
                    )
                });

                Identity {
                    addr: Addr::Spire(addr),
                    receiver,
                    metrics,
                    ready,
                    task,
                }
            }
        };

        Ok(identity)
    }
}

impl Credentials for NotifyReady {
    fn set_certificate(&mut self, leaf: DerX509, chain: Vec<DerX509>, key: Vec<u8>) -> Result<()> {
        self.store.set_certificate(leaf, chain, key)?;
        let _ = self.tx.send(true);
        Ok(())
    }
}

// === impl Identity ===

impl Identity {
    pub fn addr(&self) -> Addr {
        self.addr.clone()
    }

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

    pub fn metrics(&self) -> IdentityMetrics {
        self.metrics.clone()
    }

    pub fn run(self) -> Task {
        self.task
    }
}

// === impl Recover ===

impl<E: Into<Error>> linkerd_error::Recover<E> for Recover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
