mod receiver;
mod store;

pub use self::{receiver::Receiver, store::Store};
use boring::{
    pkey::PKey,
    x509::{store::X509StoreBuilder, X509},
};
use linkerd_error::Result;
use linkerd_identity as id;

pub fn watch(
    identity: id::Name,
    roots_pem: &str,
    key_pkcs8: &[u8],
    csr: &[u8],
) -> Result<(Store, Receiver)> {
    let roots = {
        let mut store = X509StoreBuilder::new()?;
        // FIXME(ver) This should handle a list of PEM-encoded certificates.
        let cert = X509::from_pem(roots_pem.as_bytes())?;
        store.add_cert(cert)?;
        store.build()
    };

    let key = PKey::private_key_from_pkcs8(key_pkcs8)?;

    let store = Store::new(roots, key, csr, identity);

    let rx = Receiver {};

    Ok((store, rx))
}
