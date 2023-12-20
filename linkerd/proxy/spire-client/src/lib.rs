#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod api;
mod client;

pub use self::client::Client;

use api::SvidUpdate;
use linkerd_error::{Error, Result};
use linkerd_identity::Credentials;
use linkerd_identity::Id;
use linkerd_proxy_identity_client_metrics::Metrics;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tower::Service;
use tracing::error;

pub struct Spire {
    id: Id,
    metrics: Metrics,
}

// === impl Spire ===

impl Spire {
    pub fn new(id: Id, metrics: Metrics) -> Self {
        Self { id, metrics }
    }

    pub async fn run<C, N>(self, credentials: C, mut new_client: N)
    where
        C: Credentials,
        N: Service<(), Response = watch::Receiver<SvidUpdate>>,
        N::Error: Into<Error>,
    {
        match new_client.call(()).await {
            Ok(rx) => consume_updates(&self.id, rx, credentials, &self.metrics).await,
            Err(error) => error!("could not establish SVID stream: {}", error.into()),
        }
    }
}

async fn consume_updates<C>(
    id: &Id,
    mut updates: watch::Receiver<api::SvidUpdate>,
    mut credentials: C,
    metrics: &Metrics,
) where
    C: Credentials,
{
    loop {
        let svid_update = updates.borrow_and_update().clone();
        match process_svid(&mut credentials, svid_update, id) {
            Ok(expiration) => metrics.refresh(expiration),
            Err(error) => tracing::error!("Error processing SVID update: {}", error),
        }

        if let Err(error) = updates.changed().await {
            tracing::debug!("SVID watch closed; terminating {}", error);
            return;
        }
    }
}

fn process_svid<C>(credentials: &mut C, mut update: SvidUpdate, id: &Id) -> Result<SystemTime>
where
    C: Credentials,
{
    if let Some(svid) = update.svids.remove(id) {
        use x509_parser::prelude::*;

        let (_, parsed_cert) = X509Certificate::from_der(&svid.leaf.0)?;
        let exp: u64 = parsed_cert.validity().not_after.timestamp().try_into()?;
        let exp = UNIX_EPOCH + Duration::from_secs(exp);

        credentials.set_certificate(svid.leaf, svid.intermediates, svid.private_key)?;
        return Ok(exp);
    }

    Err("could not find an SVID".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Svid;
    use linkerd_error::Result;
    use linkerd_identity::{Credentials, DerX509, Id};
    use rcgen::{Certificate, CertificateParams, SanType, SerialNumber};
    use std::collections::HashMap;
    use tokio::sync::watch;

    fn gen_cert(subject_alt_names: Vec<SanType>, serial: SerialNumber) -> DerX509 {
        let mut params = CertificateParams::default();
        params.subject_alt_names = subject_alt_names;
        params.serial_number = Some(serial);

        DerX509(
            Certificate::from_params(params)
                .expect("should generate cert")
                .serialize_der()
                .expect("should serialize"),
        )
    }

    struct MockCredentials {
        tx: watch::Sender<Option<SerialNumber>>,
    }

    impl MockCredentials {
        fn new() -> (Self, watch::Receiver<Option<SerialNumber>>) {
            let (tx, rx) = watch::channel(None);
            (Self { tx }, rx)
        }
    }

    impl Credentials for MockCredentials {
        fn set_certificate(&mut self, leaf: DerX509, _: Vec<DerX509>, _: Vec<u8>) -> Result<()> {
            let (_, cert) = x509_parser::parse_x509_certificate(&leaf.0).unwrap();
            let serial = SerialNumber::from_slice(&cert.serial.to_bytes_be());
            self.tx.send(Some(serial)).unwrap();
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_update() {
        let serial = SerialNumber::from_slice("some-serial".as_bytes());
        let spiffe_san = "spiffe://some-domain/some-workload";
        let leaf = gen_cert(vec![SanType::URI(spiffe_san.into())], serial.clone());
        let spiffe_id = Id::parse_uri("spiffe://some-domain/some-workload").expect("should parse");

        let (mut creds, mut rx) = MockCredentials::new();
        let svid = Svid {
            spiffe_id: spiffe_id.clone(),
            leaf,
            private_key: Vec::default(),
            intermediates: Vec::default(),
        };
        let mut svids = HashMap::default();
        svids.insert(svid.spiffe_id.clone(), svid);
        let update = SvidUpdate { svids };

        assert!(process_svid(&mut creds, update, &spiffe_id).is_ok());
        rx.changed().await.unwrap();
        assert!(*rx.borrow_and_update() == Some(serial));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_update() {
        let spiffe_id = Id::parse_uri("spiffe://some-domain/some-workload").expect("should parse");
        let (mut creds, mut rx) = MockCredentials::new();
        let svid = Svid {
            spiffe_id: spiffe_id.clone(),
            leaf: DerX509(Vec::default()),
            private_key: Vec::default(),
            intermediates: Vec::default(),
        };
        let mut svids = HashMap::default();
        svids.insert(svid.spiffe_id.clone(), svid);
        let update = SvidUpdate { svids };

        assert!(process_svid(&mut creds, update, &spiffe_id).is_err());
        assert!(rx.borrow_and_update().is_none());
    }
}
