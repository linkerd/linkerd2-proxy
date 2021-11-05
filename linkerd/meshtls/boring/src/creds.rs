mod receiver;
mod store;

pub use self::{receiver::Receiver, store::Store};
use boring::{
    pkey::PKey,
    ssl,
    x509::{store::X509StoreBuilder, X509},
};
use linkerd_error::Result;
use linkerd_identity as id;
use tokio::sync::watch;

pub fn watch(
    identity: id::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let roots = {
        let certs = X509::stack_from_pem(roots_pem.as_bytes())?;
        let mut store = X509StoreBuilder::new()?;
        for c in certs.into_iter() {
            store.add_cert(c)?;
        }
        store.build()
    };

    let key = PKey::private_key_from_pkcs8(key_pkcs8)?;

    let (client_tx, client_rx) = {
        let mut conn = ssl::SslConnector::builder(ssl::SslMethod::tls_client())?;
        let roots = clone_roots(&roots)?;
        conn.set_cert_store(roots);
        watch::channel(conn.build())
    };
    let (server_tx, server_rx) = {
        let acc = ssl::SslAcceptor::mozilla_intermediate_v5(ssl::SslMethod::tls_server())?;
        watch::channel(acc.build())
    };
    let rx = Receiver::new(identity.clone(), client_rx, server_rx);
    let store = Store::new(roots, key, csr, identity, client_tx, server_tx);

    Ok((store, rx))
}

/// Duplicates an `X509Store`.
///
/// For unknown reasons, `X509Store` does not implement `Clone`. What this function presupposes is:
/// but, what if it did?
fn clone_roots(orig: &boring::x509::store::X509Store) -> Result<boring::x509::store::X509Store> {
    // X509Store does not implement clone, so we need to manually copy it.
    let mut roots = boring::x509::store::X509StoreBuilder::new()?;
    for obj in orig.objects() {
        if let Some(c) = obj.x509() {
            roots.add_cert(c.to_owned())?;
        }
    }
    Ok(roots.build())
}
