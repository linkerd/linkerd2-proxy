use crate::spire;

pub use linkerd_app_core::identity::{client, Id};
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity::{
        client::linkerd::Certify, creds, CertMetrics, Credentials, DerX509, Mode, WithCertMetrics,
    },
    metrics::{prom, ControlHttp as ClientMetrics},
    Error, Result,
};
use std::{future::Future, pin::Pin, time::SystemTime};
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Debug, thiserror::Error)]
#[error("linkerd identity requires a TLS Id and server name to be the same")]
pub struct TlsIdAndServerNameNotMatching(());

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Linkerd {
        client: control::Config,
        certify: client::linkerd::Config,
        tls: TlsParams,
    },
    Spire {
        client: spire::Config,
        tls: TlsParams,
    },
}

#[derive(Clone, Debug)]
pub struct TlsParams {
    pub id: Id,
    pub server_name: dns::Name,
    pub trust_anchors_pem: String,
}

pub struct Identity {
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
        let cert_metrics =
            CertMetrics::register(registry.sub_registry_with_prefix("identity_cert"));

        Ok(match self {
            Self::Linkerd {
                client,
                certify,
                tls,
            } => {
                // TODO: move this validation into env.rs
                let name = match (&tls.id, &tls.server_name) {
                    (Id::Dns(id), sni) if id == sni => id.clone(),
                    (_id, _sni) => {
                        return Err(TlsIdAndServerNameNotMatching(()).into());
                    }
                };

                let certify = Certify::from(certify);
                let (store, receiver, ready) = watch(tls, cert_metrics)?;

                let task = {
                    let addr = client.addr.clone();
                    let svc = client.build(
                        dns,
                        client_metrics,
                        registry.sub_registry_with_prefix("control_identity"),
                        receiver.new_client(),
                    );

                    Box::pin(certify.run(name, store, svc).instrument(
                        tracing::info_span!("identity", server.addr = %addr).or_current(),
                    ))
                };
                Identity {
                    receiver,
                    ready,
                    task,
                }
            }
            Self::Spire { client, tls } => {
                let addr = client.socket_addr.clone();
                let spire = spire::client::Spire::new(tls.id.clone());

                let (store, receiver, ready) = watch(tls, cert_metrics)?;
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
    tls: TlsParams,
    metrics: CertMetrics,
) -> Result<(
    WithCertMetrics<NotifyReady>,
    creds::Receiver,
    watch::Receiver<bool>,
)> {
    let (tx, ready) = watch::channel(false);
    let (store, receiver) =
        Mode::default().watch(tls.id, tls.server_name, &tls.trust_anchors_pem)?;
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

// === impl Recover ===

impl<E: Into<Error>> linkerd_error::Recover<E> for Recover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
