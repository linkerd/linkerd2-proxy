pub use linkerd_app_core::identity::{client, Id};
use linkerd_app_core::{
    control, dns,
    identity::{
        client::linkerd::Certify, creds, CertMetrics, Credentials, DerX509, Mode, WithCertMetrics,
    },
    metrics::{prom, ControlHttp as ClientMetrics},
    Result,
};
use std::time::SystemTime;
use tokio::sync::watch;
use tracing::Instrument;

/// Wraps a credential with a watch sender that notifies receivers when the store has been updated
/// at least once.
struct NotifyReady {
    store: creds::Store,
    tx: watch::Sender<bool>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub client: control::Config,
    pub certify: client::linkerd::Config,
    pub tls: super::TlsParams,
}

impl Config {
    pub fn build(
        self,
        cert_metrics: CertMetrics,
        dns: dns::Resolver,
        client_metrics: ClientMetrics,
        registry: &mut prom::Registry,
    ) -> Result<super::Identity> {
        let Self {
            client,
            certify,
            tls,
        } = self;
        // TODO: move this validation into env.rs
        let name = match (&tls.id, &tls.server_name) {
            (Id::Dns(id), sni) if id == sni => id.clone(),
            (_id, _sni) => {
                return Err(super::TlsIdAndServerNameNotMatching(()).into());
            }
        };

        let roots = DerX509(pkix::pem::pem_to_der(&tls.trust_anchors_pem, None).unwrap());
        let certify = Certify::from(certify);
        let (store, receiver, mut obtained_cert) = watch(tls, cert_metrics)?;
        let (ready_tx, ready_rx) = watch::channel(None);

        let task =
            {
                let addr = client.addr.clone();
                let svc = client.build(
                    dns,
                    client_metrics,
                    registry.sub_registry_with_prefix("control_identity"),
                    receiver.new_client(),
                );

                Box::pin(async move {
                    tokio::spawn(certify.run(name, roots, store, svc).instrument(
                        tracing::info_span!("identity", server.addr = %addr).or_current(),
                    ));

                    // make sure to wait while a cert has been obtained
                    while !*obtained_cert.borrow_and_update() {
                        obtained_cert
                            .changed()
                            .await
                            .expect("identity sender must be held");
                    }

                    let _ = ready_tx.send(Some(receiver));
                })
            };

        Ok(super::Identity {
            ready: ready_rx,
            task,
        })
    }
}

fn watch(
    tls: super::TlsParams,
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
        roots: DerX509,
    ) -> Result<()> {
        self.store.set_certificate(leaf, chain, key, exp, roots)?;
        let _ = self.tx.send(true);
        Ok(())
    }
}
