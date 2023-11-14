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

#[cfg(test)]
mod tests {
    use super::verify_id;
    use linkerd_meshtls_test_util as test_util;
    use tokio_rustls::rustls::Certificate;

    fn vec_to_cert(data: Vec<u8>) -> Certificate {
        Certificate(data)
    }

    #[test]
    fn cert_with_dns_san_matches_dns_id() {
        test_util::cert_with_dns_san_matches_dns_id(verify_id, vec_to_cert);
    }

    #[test]
    fn cert_with_dns_san_does_not_match_dns_id() {
        test_util::cert_with_dns_san_does_not_match_dns_id(verify_id, vec_to_cert);
    }

    #[test]
    fn cert_with_uri_san_does_not_match_dns_id() {
        test_util::cert_with_uri_san_does_not_match_dns_id(verify_id, vec_to_cert);
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_dns_id() {
        test_util::cert_with_no_san_does_not_verify_for_dns_id(verify_id, vec_to_cert);
    }

    #[test]
    fn cert_with_dns_multiple_sans_one_matches_dns_id() {
        test_util::cert_with_dns_multiple_sans_one_matches_dns_id(verify_id, vec_to_cert);
    }

    #[test]
    fn cert_with_dns_multiple_sans_none_matches_dns_id() {
        test_util::cert_with_dns_multiple_sans_none_matches_dns_id(verify_id, vec_to_cert);
    }
}
