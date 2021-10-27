pub use linkerd_app_core::identity::*;
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity,
    metrics::ControlHttp as Metrics,
    Error, Result,
};
use std::{future::Future, pin::Pin};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub certify: certify::Config,
    pub documents: Documents,
}

#[derive(Clone)]
pub struct Documents {
    pub id: LocalId,
    pub trust_anchors_pem: String,
    pub key_pkcs8: Vec<u8>,
    pub csr_der: Vec<u8>,
}

pub struct Identity {
    addr: control::ControlAddr,
    receiver: identity::creds::Receiver,
    ready: watch::Receiver<bool>,
    metrics: identity::Metrics,
    task: Task,
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

/// Wraps a credential with a watch sender that notifies receivers when the store has been updated
/// at least once.
struct NotifyReady {
    store: identity::creds::Store,
    tx: watch::Sender<bool>,
}

// === impl Config ===

impl Config {
    pub fn build(self, dns: dns::Resolver, client_metrics: Metrics) -> Result<Identity> {
        let (store, receiver) = identity::creds::watch(
            (*self.documents.id).clone(),
            &self.documents.trust_anchors_pem,
            &self.documents.key_pkcs8,
            &self.documents.csr_der,
        )?;

        let certify = identity::Certify::from(self.certify);
        let metrics = certify.metrics();

        let addr = self.control.addr.clone();

        let (tx, ready) = watch::channel(false);

        // Save to be spawned on an auxiliary runtime.
        let task = Box::pin({
            let addr = addr.clone();
            let svc = self
                .control
                .build(dns, client_metrics, receiver.new_client());

            certify
                .run(NotifyReady { store, tx }, svc)
                .instrument(tracing::debug_span!("identity", server.addr = %addr).or_current())
        });

        Ok(Identity {
            addr,
            receiver,
            metrics,
            ready,
            task,
        })
    }
}

impl identity::Credentials for NotifyReady {
    #[inline]
    fn get_dns_name(&self) -> &Name {
        self.store.get_dns_name()
    }

    #[inline]
    fn gen_certificate_signing_request(&mut self) -> identity::DerX509 {
        self.store.gen_certificate_signing_request()
    }

    fn set_certificate(
        &mut self,
        leaf: identity::DerX509,
        chain: Vec<identity::DerX509>,
        expiry: std::time::SystemTime,
    ) -> Result<()> {
        self.store.set_certificate(leaf, chain, expiry)?;
        let _ = self.tx.send(true);
        Ok(())
    }
}

// === impl Documents ===

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents")
            .field("id", &self.id)
            .field("trust_anchors_pem", &self.trust_anchors_pem)
            .finish()
    }
}

// === impl Identity ===

impl Identity {
    pub fn addr(&self) -> control::ControlAddr {
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

    pub fn receiver(&self) -> identity::creds::Receiver {
        self.receiver.clone()
    }

    pub fn metrics(&self) -> identity::Metrics {
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
