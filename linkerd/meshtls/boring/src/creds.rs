mod receiver;
mod store;

pub use self::{receiver::Receiver, store::Store};
use boring::{
    pkey::{PKey, Private},
    ssl,
    x509::{store::X509StoreBuilder, X509},
};
use linkerd_dns_name as dns;
use linkerd_error::Result;
use linkerd_identity as id;
use std::sync::Arc;
use tokio::sync::watch;

pub fn watch(
    local_id: id::Id,
    server_name: dns::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let creds = {
        let roots = X509::stack_from_pem(roots_pem.as_bytes())?;
        let key = PKey::private_key_from_pkcs8(key_pkcs8)?;
        Arc::new(BaseCreds { roots, key })
    };

    let (tx, rx) = watch::channel(Creds::from(creds.clone()));
    let rx = Receiver::new(local_id.clone(), server_name, rx);
    let store = Store::new(creds, csr, local_id, tx);

    Ok((store, rx))
}

pub(crate) struct Creds {
    base: Arc<BaseCreds>,
    certs: Option<Certs>,
}

struct BaseCreds {
    roots: Vec<X509>,
    key: PKey<Private>,
}

struct Certs {
    leaf: X509,
    intermediates: Vec<X509>,
}

pub(crate) type CredsRx = watch::Receiver<Creds>;

type CredsTx = watch::Sender<Creds>;

// === impl Creds ===

impl From<Arc<BaseCreds>> for Creds {
    fn from(base: Arc<BaseCreds>) -> Self {
        Self { base, certs: None }
    }
}

impl Creds {
    // TODO(ver) Specify certificate types, signing algorithms, cipher suites..
    pub(crate) fn acceptor(&self, alpn_protocols: &[Vec<u8>]) -> Result<ssl::SslAcceptor> {
        // mozilla_intermediate_v5 is the only variant that enables TLSv1.3, so we use that.
        let mut conn = ssl::SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls_server())?;

        // Force use of TLSv1.3.
        conn.set_options(ssl::SslOptions::NO_TLSV1_2);
        conn.clear_options(ssl::SslOptions::NO_TLSV1_3);

        let roots = self.root_store()?;
        tracing::debug!(
            roots = ?self
                .base
                .roots
                .iter()
                .filter_map(|c| super::fingerprint(c))
                .collect::<Vec<_>>(),
            "Configuring acceptor roots",
        );
        conn.set_cert_store(roots);

        // Ensure that client certificates are validated when present.
        conn.set_verify(ssl::SslVerifyMode::PEER);

        if let Some(certs) = &self.certs {
            tracing::debug!(
                cert = ?super::fingerprint(&certs.leaf),
                "Configuring acceptor certificate",
            );
            conn.set_private_key(&self.base.key)?;
            conn.set_certificate(&certs.leaf)?;
            conn.check_private_key()?;
            for c in &certs.intermediates {
                conn.add_extra_chain_cert(c.to_owned())?;
            }
        }

        if !alpn_protocols.is_empty() {
            let p = serialize_alpn(alpn_protocols)?;
            conn.set_alpn_protos(&p)?;
        }

        Ok(conn.build())
    }

    // TODO(ver) Specify certificate types, signing algorithms, cipher suites..
    pub(crate) fn connector(&self, alpn_protocols: &[Vec<u8>]) -> Result<ssl::SslConnector> {
        // XXX(ver) This function reads from the environment and/or the filesystem. This likely is
        // at best wasteful and at worst unsafe (if another thread were to mutate these environment
        // variables simultaneously, for instance). Unfortunately, the boring APIs don't really give
        // us an alternative AFAICT.
        let mut conn = ssl::SslConnector::builder(ssl::SslMethod::tls_client())?;

        // Explicitly enable use of TLSv1.3
        conn.set_options(ssl::SslOptions::NO_TLSV1 | ssl::SslOptions::NO_TLSV1_1);
        // XXX(ver) if we disable use of TLSv1.2, connections just hang.
        //conn.set_options(ssl::SslOptions::NO_TLSV1_2);
        conn.clear_options(ssl::SslOptions::NO_TLSV1_3);

        tracing::debug!(
            roots = ?self
                .base
                .roots
                .iter()
                .filter_map(|c| super::fingerprint(c))
                .collect::<Vec<_>>(),
            "Configuring connector roots",
        );
        let roots = self.root_store()?;
        conn.set_cert_store(roots);

        if let Some(certs) = &self.certs {
            tracing::debug!(
                cert = ?super::fingerprint(&certs.leaf),
                intermediates = %certs.intermediates.len(),
                "Configuring connector certificate",
            );
            conn.set_private_key(&self.base.key)?;
            conn.set_certificate(&certs.leaf)?;
            conn.check_private_key()?;
            for c in &certs.intermediates {
                conn.add_extra_chain_cert(c.to_owned())?;
            }
        }

        if !alpn_protocols.is_empty() {
            let p = serialize_alpn(alpn_protocols)?;
            conn.set_alpn_protos(&p)?;
        }

        Ok(conn.build())
    }

    fn root_store(&self) -> Result<boring::x509::store::X509Store> {
        let mut store = X509StoreBuilder::new()?;
        for c in &self.base.roots {
            store.add_cert(c.to_owned())?;
        }

        Ok(store.build())
    }
}

/// Encodes a list of ALPN protocols into a slice of bytes.
///
/// `boring` requires that the list of protocols be encoded in the wire format.
fn serialize_alpn(protocols: &[Vec<u8>]) -> Result<Vec<u8>> {
    // Allocate a buffer to hold the encoded protocols.
    let mut bytes = {
        // One additional byte for each protocol's length prefix.
        let cap = protocols.len() + protocols.iter().map(Vec::len).sum::<usize>();
        Vec::with_capacity(cap)
    };

    // Encode each protocol as a length-prefixed string.
    for p in protocols {
        if p.is_empty() {
            continue;
        }
        if p.len() > 255 {
            return Err("ALPN protocols must be less than 256 bytes".into());
        }
        bytes.push(p.len() as u8);
        bytes.extend(p);
    }

    Ok(bytes)
}

#[cfg(test)]
#[test]
fn test_serialize_alpn() {
    assert_eq!(serialize_alpn(&[b"h2".to_vec()]).unwrap(), b"\x02h2");
    assert_eq!(
        serialize_alpn(&[b"h2".to_vec(), b"http/1.1".to_vec()]).unwrap(),
        b"\x02h2\x08http/1.1"
    );
    assert_eq!(
        serialize_alpn(&[b"h2".to_vec(), b"http/1.1".to_vec()]).unwrap(),
        b"\x02h2\x08http/1.1"
    );
    assert_eq!(
        serialize_alpn(&[b"h2".to_vec(), vec![], b"http/1.1".to_vec()]).unwrap(),
        b"\x02h2\x08http/1.1"
    );

    assert!(serialize_alpn(&[(0..255).collect()]).is_ok());
    assert!(serialize_alpn(&[(0..=255).collect()]).is_err());
}
