use std::convert::TryFrom;
use std::sync::Arc;
use std::time::SystemTime;
use tokio_rustls::rustls::{
    self,
    client::{self, ServerCertVerified, ServerCertVerifier},
    server::ParsedCertificate,
    Certificate, RootCertStore, ServerName,
};
use tracing::trace;

pub(crate) struct AnySanVerifier(Arc<RootCertStore>);

impl AnySanVerifier {
    pub(crate) fn new(roots: impl Into<Arc<RootCertStore>>) -> Self {
        Self(roots.into())
    }
}

// This is derived from `rustls::client::WebPkiVerifier`.
//
//   Copyright (c) 2016, Joseph Birr-Pixton <jpixton@gmail.com>
// https://github.com/rustls/rustls/blob/ccb79947a4811412ee7dcddcd0f51ea56bccf101/rustls/src/webpki/server_verifier.rs#L239
//
// The only difference is that we omit the step that performs
// DNS SAN validation. The reason for that stems from the fact that
// SAN validation in rustls is limited to DNS SANs only while we
// want to support alternative SAN types (e.g. URI).
impl ServerCertVerifier for AnySanVerifier {
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
            trace!("Unvalidated OCSP response: {ocsp_response:?}");
        }

        Ok(ServerCertVerified::assertion())
    }
}
