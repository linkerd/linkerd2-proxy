use crate::creds::params::SUPPORTED_SIG_ALGS;
use std::{convert::TryFrom, sync::Arc};
use tokio_rustls::rustls::{
    self,
    client::{
        self,
        danger::{ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::ParsedCertificate,
    RootCertStore,
};
use tracing::trace;

#[derive(Debug)]
pub(crate) struct AnySanVerifier {
    roots: Arc<RootCertStore>,
}

impl AnySanVerifier {
    pub(crate) fn new(roots: impl Into<Arc<RootCertStore>>) -> Self {
        Self {
            roots: roots.into(),
        }
    }
}

// This is derived from `rustls::client::WebPkiServerVerifier`.
//
//   Copyright (c) 2016, Joseph Birr-Pixton <jpixton@gmail.com>
// https://github.com/rustls/rustls/blob/v/0.23.15/rustls/src/webpki/server_verifier.rs#L134
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
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;

        client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            SUPPORTED_SIG_ALGS.all,
        )?;

        if !ocsp_response.is_empty() {
            trace!("Unvalidated OCSP response: {ocsp_response:?}");
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<client::danger::HandshakeSignatureValid, rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls12_signature(message, cert, dss, SUPPORTED_SIG_ALGS)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<client::danger::HandshakeSignatureValid, rustls::Error> {
        tokio_rustls::rustls::crypto::verify_tls13_signature(message, cert, dss, SUPPORTED_SIG_ALGS)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        SUPPORTED_SIG_ALGS.supported_schemes()
    }
}
