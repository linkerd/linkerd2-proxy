pub use linkerd_app_core::identity::{
    client::{certify, TokenSource},
    Id,
};
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity::{client::Certify, creds, CertMetrics, Credentials, DerX509, Mode, WithCertMetrics},
    metrics::{prom, ControlHttp as ClientMetrics},
    Error, Result,
};
use std::{future::Future, pin::Pin, time::SystemTime};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub certify: certify::Config,
    pub params: TlsParams,
}

#[derive(Clone, Debug)]
pub struct TlsParams {
    pub server_id: Id,
    pub server_name: dns::Name,
    pub trust_anchors_pem: String,
}

pub struct Identity {
    addr: control::ControlAddr,
    receiver: creds::Receiver,
    ready: watch::Receiver<bool>,
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

        let certify = Certify::from(self.certify);

        let addr = self.control.addr.clone();

        let (tx, ready) = watch::channel(false);

        // Save to be spawned on an auxiliary runtime.
        let task = Box::pin({
            let addr = addr.clone();
            let svc = self.control.build(
                dns,
                client_metrics,
                registry.sub_registry_with_prefix("control_identity"),
                receiver.new_client(),
            );

            let cert_metrics =
                CertMetrics::register(registry.sub_registry_with_prefix("identity_cert"));
            let cred = WithCertMetrics::new(cert_metrics, NotifyReady { store, tx });

            certify
                .run(name, cred, svc)
                .instrument(tracing::debug_span!("identity", server.addr = %addr).or_current())
        });

        Ok(Identity {
            addr,
            receiver,
            ready,
            task,
        })
    }
}

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

    pub fn receiver(&self) -> creds::Receiver {
        self.receiver.clone()
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
