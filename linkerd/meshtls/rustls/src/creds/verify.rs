use linkerd_identity as id;
use std::convert::TryFrom;
use std::time::SystemTime;
use std::{io, sync::Arc};
use tokio_rustls::rustls::{
    self,
    client::{self, ServerCertVerified, ServerCertVerifier},
    server::ParsedCertificate,
    Certificate, RootCertStore, ServerName,
};
use tracing::trace;

pub(crate) struct AnySANVerifier(Arc<RootCertStore>);

impl AnySANVerifier {
    pub(crate) fn new(roots: impl Into<Arc<RootCertStore>>) -> Self {
        Self(roots.into())
    }
}

impl ServerCertVerifier for AnySANVerifier {
    /// Will verify the certificate is valid in the following ways:
    /// - Signed by a  trusted `RootCertStore` CA
    /// - Not Expired
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        _: &ServerName,
        _: &mut dyn Iterator<Item = &[u8]>,
        ocsp_response: &[u8],
        now: SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;

        client::verify_server_cert_signed_by_trust_anchor(&cert, &self.0, intermediates, now)?;

        if !ocsp_response.is_empty() {
            trace!("Unvalidated OCSP response: {:?}", ocsp_response.to_vec());
        }

        Ok(ServerCertVerified::assertion())
    }
}

pub(crate) fn verify_id(end_entity: &Certificate, id: &id::Id) -> io::Result<()> {
    // XXX(zahari) Currently this logic is the same as in rustls::client::WebPkiVerifier.
    // This will eventually change to support SPIFFE IDs
    let cert = ParsedCertificate::try_from(end_entity)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let server_name =
        rustls::ServerName::try_from(id.to_str().as_ref()).expect("id must be a valid DNS name");

    rustls::client::verify_server_name(&cert, &server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}
