use crate::spire;

pub use linkerd_app_core::identity::{client, Id};
use linkerd_app_core::{
    dns,
    identity::{client_identity, creds, CertMetrics, Credentials, DerX509, Mode, WithCertMetrics},
    Result,
};
use spire::client as spire_client;
use std::time::SystemTime;
use tokio::sync::watch;
use tracing::Instrument;

#[derive(Debug, thiserror::Error)]
#[error("linkerd identity requires a TLS Id and server name to be the same")]
pub struct TlsIdAndServerNameNotMatching(());

#[derive(Clone, Debug)]
pub struct Config {
    pub client: spire::Config,
    pub server_name: dns::Name,
}

impl Config {
    pub fn build(self, cert_metrics: CertMetrics) -> Result<super::Identity> {
        let addr = self.client.socket_addr.clone();
        let (store, ready_rx) = SpireInit::new(cert_metrics, self.server_name);
        let task = Box::pin(
            spire_client::run(store, spire::Client::from(self.client))
                .instrument(tracing::info_span!("spire", server.addr = %addr).or_current()),
        );

        Ok(super::Identity {
            ready: ready_rx,
            task,
        })
    }
}

struct SpireInit {
    store: Option<WithCertMetrics<creds::Store>>,
    tx: watch::Sender<Option<creds::Receiver>>,
    cert_metrics: CertMetrics,
    server_name: dns::Name,
}

impl SpireInit {
    fn new(
        cert_metrics: CertMetrics,
        server_name: dns::Name,
    ) -> (Self, watch::Receiver<Option<creds::Receiver>>) {
        let (tx, rx) = watch::channel(None);
        let init = Self {
            store: None,
            tx,
            cert_metrics,
            server_name,
        };

        (init, rx)
    }

    fn make_tls_params(&self, roots: DerX509, leaf: DerX509) -> super::TlsParams {
        let id = client_identity(&leaf).unwrap();
        let trust_anchors_pem =
            pkix::pem::der_to_pem(roots.0.as_slice(), pkix::pem::PEM_CERTIFICATE);

        super::TlsParams {
            id,
            server_name: self.server_name.clone(),
            trust_anchors_pem,
        }
    }
}

impl Credentials for SpireInit {
    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        key: Vec<u8>,
        exp: SystemTime,
        roots: DerX509,
    ) -> Result<()> {
        match &mut self.store {
            Some(store) => store.set_certificate(leaf, chain, key, exp, roots),
            None => {
                let tls = self.make_tls_params(roots.clone(), leaf.clone());
                let (store, receiver) =
                    Mode::default().watch(tls.id, tls.server_name, &tls.trust_anchors_pem)?;
                let mut store = WithCertMetrics::new(self.cert_metrics.clone(), store);

                store.set_certificate(leaf, chain, key, exp, roots)?;
                self.store = Some(store);
                let _ = self.tx.send(Some(receiver));
                Ok(())
            }
        }
    }
}
