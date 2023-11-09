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
    use linkerd_identity as id;
    use rcgen::generate_simple_self_signed;
    use std::str::FromStr;
    use tokio_rustls::rustls::Certificate;

    fn generate_cert_with_name(name: Option<&str>) -> Certificate {
        let sans = name.map(|s| vec![s.into()]).unwrap_or_default();

        let cert_data = generate_simple_self_signed(sans)
            .expect("should generate cert")
            .serialize_der()
            .expect("should serialize");

        Certificate(cert_data)
    }

    #[test]
    fn cert_with_dns_san_matches_dns_id() {
        let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_name(Some(dns_name));
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_san_does_not_match_dns_id() {
        let dns_name_cert = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_name(Some(dns_name_cert));
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_uri_san_does_not_match_dns_id() {
        let uri_name_cert = "spiffe://some-trust-comain/some-system/some-component";
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_name(Some(uri_name_cert));
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_dns_id() {
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_name(None);
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }
}
