#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod api;

pub use api::{Api, SvidUpdate};
use linkerd_error::{Error, Result};
use linkerd_identity::Credentials;
use linkerd_identity::Id;
use std::fmt::Display;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::watch;
use tower::Service;
use tracing::error;

pub struct Spire {
    id: Id,
}

// === impl Spire ===

impl Spire {
    pub fn new(id: Id) -> Self {
        Self { id }
    }

    pub async fn run<C, S>(self, credentials: C, mut client: S)
    where
        C: Credentials,
        S: Service<(), Response = tonic::Response<watch::Receiver<SvidUpdate>>>,
        S::Error: Into<Error> + Display,
    {
        match client.call(()).await {
            Ok(rsp) => consume_updates(&self.id, rsp.into_inner(), credentials).await,
            Err(error) => error!(%error, "could not establish SVID stream"),
        }
    }
}

async fn consume_updates<C>(
    id: &Id,
    mut updates: watch::Receiver<api::SvidUpdate>,
    mut credentials: C,
) where
    C: Credentials,
{
    loop {
        let svid_update = updates.borrow_and_update().clone();
        if let Err(error) = process_svid(&mut credentials, svid_update, id) {
            tracing::error!("Error processing SVID update: {}", error);
        }

        if let Err(error) = updates.changed().await {
            tracing::debug!("SVID watch closed; terminating {}", error);
            return;
        }
    }
}

fn process_svid<C>(credentials: &mut C, mut update: SvidUpdate, id: &Id) -> Result<()>
where
    C: Credentials,
{
    if let Some(svid) = update.svids.remove(id) {
        use x509_parser::prelude::*;

        let (_, parsed_cert) = X509Certificate::from_der(&svid.leaf.0)?;
        let exp: u64 = parsed_cert.validity().not_after.timestamp().try_into()?;
        let exp = UNIX_EPOCH + Duration::from_secs(exp);

        return credentials.set_certificate(svid.leaf, svid.intermediates, svid.private_key, exp);
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
    use std::{collections::HashMap, time::SystemTime};
    use tokio::sync::watch;

    fn gen_svid(id: Id, subject_alt_names: Vec<SanType>, serial: SerialNumber) -> Svid {
        let mut params = CertificateParams::default();
        params.subject_alt_names = subject_alt_names;
        params.serial_number = Some(serial);

        Svid {
            spiffe_id: id,
            leaf: DerX509(
                Certificate::from_params(params)
                    .expect("should generate cert")
                    .serialize_der()
                    .expect("should serialize"),
            ),
            private_key: Vec::default(),
            intermediates: Vec::default(),
        }
    }

    fn svid_update(svids: Vec<Svid>) -> SvidUpdate {
        let mut svids_map = HashMap::default();
        for svid in svids.into_iter() {
            svids_map.insert(svid.spiffe_id.clone(), svid);
        }

        SvidUpdate { svids: svids_map }
    }

    struct MockClient {
        rx: watch::Receiver<SvidUpdate>,
    }

    impl MockClient {
        fn new(init: SvidUpdate) -> (Self, watch::Sender<SvidUpdate>) {
            let (tx, rx) = watch::channel(init);
            (Self { rx }, tx)
        }
    }

    impl tower::Service<()> for MockClient {
        type Response = tonic::Response<watch::Receiver<SvidUpdate>>;
        type Error = Error;
        // type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;
        type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: ()) -> Self::Future {
            let rsp = tonic::Response::new(self.rx.clone());
            Box::pin(futures::future::ready(Ok(rsp)))
        }
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
        fn set_certificate(
            &mut self,
            leaf: DerX509,
            _: Vec<DerX509>,
            _: Vec<u8>,
            _: SystemTime,
        ) -> Result<()> {
            let (_, cert) = x509_parser::parse_x509_certificate(&leaf.0).unwrap();
            let serial = SerialNumber::from_slice(&cert.serial.to_bytes_be());
            self.tx.send(Some(serial)).unwrap();
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_updates() {
        let spiffe_san = "spiffe://some-domain/some-workload";
        let spiffe_id = Id::parse_uri("spiffe://some-domain/some-workload").expect("should parse");

        let (creds, mut creds_rx) = MockCredentials::new();

        let spire = Spire::new(spiffe_id.clone());

        let serial_1 = SerialNumber::from_slice("some-serial-1".as_bytes());
        let update_1 = svid_update(vec![gen_svid(
            spiffe_id.clone(),
            vec![SanType::URI(spiffe_san.into())],
            serial_1.clone(),
        )]);

        let (client, svid_tx) = MockClient::new(update_1);
        tokio::spawn(spire.run(creds, client));

        creds_rx.changed().await.unwrap();
        assert!(*creds_rx.borrow_and_update() == Some(serial_1));

        let serial_2 = SerialNumber::from_slice("some-serial-2".as_bytes());
        let update_2 = svid_update(vec![gen_svid(
            spiffe_id.clone(),
            vec![SanType::URI(spiffe_san.into())],
            serial_2.clone(),
        )]);

        svid_tx.send(update_2).expect("should send");

        creds_rx.changed().await.unwrap();
        assert!(*creds_rx.borrow_and_update() == Some(serial_2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_update_empty_cert() {
        let spiffe_san = "spiffe://some-domain/some-workload";
        let spiffe_id = Id::parse_uri("spiffe://some-domain/some-workload").expect("should parse");

        let (creds, mut creds_rx) = MockCredentials::new();

        let spire = Spire::new(spiffe_id.clone());

        let serial_1 = SerialNumber::from_slice("some-serial-1".as_bytes());
        let update_1 = svid_update(vec![gen_svid(
            spiffe_id.clone(),
            vec![SanType::URI(spiffe_san.into())],
            serial_1.clone(),
        )]);

        let (client, svid_tx) = MockClient::new(update_1);
        tokio::spawn(spire.run(creds, client));

        creds_rx.changed().await.unwrap();
        assert!(*creds_rx.borrow_and_update() == Some(serial_1.clone()));

        let invalid_svid = Svid {
            spiffe_id: spiffe_id.clone(),
            leaf: DerX509(Vec::default()),
            private_key: Vec::default(),
            intermediates: Vec::default(),
        };

        let mut update_sent = svid_tx.subscribe();
        let update_2 = svid_update(vec![invalid_svid]);
        svid_tx.send(update_2).expect("should send");

        update_sent.changed().await.unwrap();

        assert!(!creds_rx.has_changed().unwrap());
        assert!(*creds_rx.borrow_and_update() == Some(serial_1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_valid_update_non_matching_id() {
        let spiffe_san = "spiffe://some-domain/some-workload";
        let spiffe_san_wrong = "spiffe://some-domain/wrong";

        let spiffe_id = Id::parse_uri("spiffe://some-domain/some-workload").expect("should parse");
        let spiffe_id_wrong = Id::parse_uri("spiffe://some-domain/wrong").expect("should parse");

        let (creds, mut creds_rx) = MockCredentials::new();

        let spire = Spire::new(spiffe_id.clone());

        let serial_1 = SerialNumber::from_slice("some-serial-1".as_bytes());
        let update_1 = svid_update(vec![gen_svid(
            spiffe_id.clone(),
            vec![SanType::URI(spiffe_san.into())],
            serial_1.clone(),
        )]);

        let (client, svid_tx) = MockClient::new(update_1);
        tokio::spawn(spire.run(creds, client));

        creds_rx.changed().await.unwrap();
        assert!(*creds_rx.borrow_and_update() == Some(serial_1.clone()));

        let serial_2 = SerialNumber::from_slice("some-serial-2".as_bytes());
        let mut update_sent = svid_tx.subscribe();
        let update_2 = svid_update(vec![gen_svid(
            spiffe_id_wrong,
            vec![SanType::URI(spiffe_san_wrong.into())],
            serial_2.clone(),
        )]);

        svid_tx.send(update_2).expect("should send");

        update_sent.changed().await.unwrap();

        assert!(!creds_rx.has_changed().unwrap());
        assert!(*creds_rx.borrow_and_update() == Some(serial_1));
    }
}
