use linkerd_error::Result;
use linkerd_identity::Id;
use std::io;

fn extract_ids_from_cert(cert: &[u8]) -> Result<Vec<Id>> {
    use x509_parser::prelude::*;
    let (_, c) = X509Certificate::from_der(cert)?;
    let names = c
        .subject_alternative_name()?
        .map(|x| &x.value.general_names);

    if let Some(names) = names {
        return Ok(names
            .iter()
            .filter_map(|n| {
                let id = match n {
                    GeneralName::DNSName(dns) => {
                        if *dns == "*" {
                            // Wildcards can perhaps be handled in a future path...
                            return None;
                        }
                        Id::parse_dns_name(dns)
                    }
                    GeneralName::URI(uri) => Id::parse_uri(uri),
                    _ => return None,
                };

                match id {
                    Ok(id) => Some(id),
                    Err(error) => {
                        tracing::warn!(%error, "SAN name {} could not be parsed", n);
                        None
                    }
                }
            })
            .collect());
    }
    Ok(Vec::default())
}

pub fn client_identity(cert: &[u8]) -> Option<Id> {
    let ids = extract_ids_from_cert(cert)
        .map_err(|error| tracing::warn!(%error, "Failed to extract tls id from client end cert"))
        .ok()?;

    ids.first().cloned()
}

pub fn verify_id(cert: &[u8], expected_id: &Id) -> io::Result<()> {
    let ids = extract_ids_from_cert(cert)
        .map_err(|error| tracing::warn!(%error, "Failed to extract tls id from client end cert"))
        .ok()
        .unwrap_or_default();

    if ids.contains(expected_id) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "certificate does not match TLS identity",
    ))
}

#[cfg(test)]
mod tests {
    use crate::client_identity;
    use crate::verify_id;
    use linkerd_identity::Id;
    use rcgen::{Certificate, CertificateParams, SanType};

    fn generate_cert_with_names(subject_alt_names: Vec<SanType>) -> Vec<u8> {
        let mut params = CertificateParams::default();
        params.subject_alt_names = subject_alt_names;

        Certificate::from_params(params)
            .expect("should generate cert")
            .serialize_der()
            .expect("should serialize")
    }

    #[test]
    pub fn cert_with_dns_san_matches_dns_id() {
        let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![SanType::DnsName(dns_name.into())]);
        let id = Id::parse_dns_name(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_spiffe_san_matches_spiffe_id() {
        let spiffe_uri = "spiffe://identity.linkerd.cluster.local/ns/ns1/sa/foo";
        let cert = generate_cert_with_names(vec![SanType::URI(spiffe_uri.into())]);
        let id = Id::parse_uri(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    pub fn cert_with_dns_san_does_not_match_dns_id() {
        let dns_name_cert = vec![SanType::DnsName(
            "foo.ns1.serviceaccount.identity.linkerd.cluster.local".into(),
        )];
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(dns_name_cert);
        let id = Id::parse_dns_name(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_dns_san_does_not_match_spiffe_id() {
        let dns_name_cert = vec![SanType::DnsName(
            "bar.ns1.serviceaccount.identity.linkerd.cluster.local".into(),
        )];
        let spiffe_uri = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(dns_name_cert);
        let id = Id::parse_uri(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_dns_id() {
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![]);
        let id = Id::parse_dns_name(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_spiffe_id() {
        let spiffe_uri = "spiffe://some-trust-comain/some-system/some-component";
        let cert = generate_cert_with_names(vec![]);
        let id = Id::parse_uri(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_multiple_sans_one_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(foo_dns_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::URI(spiffe_id.into()),
        ]);
        let id = Id::parse_dns_name(foo_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_multiple_sans_one_matches_spiffe_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(foo_dns_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::URI(spiffe_id.into()),
        ]);
        let id = Id::parse_uri(spiffe_id).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_multiple_sans_none_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(foo_dns_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::URI(spiffe_id.into()),
        ]);
        let id = Id::parse_dns_name(nar_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_multiple_sans_none_matches_spiffe_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(foo_dns_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::DnsName(nar_dns_id.into()),
        ]);
        let id = Id::parse_uri(spiffe_id).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn can_extract_spiffe_client_identity_one_san() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![SanType::URI(spiffe_id.into())]);
        let id = Id::parse_uri(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_spiffe_client_identity_many_sans() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(vec![
            SanType::URI(spiffe_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::DnsName(nar_dns_id.into()),
        ]);
        let id = Id::parse_uri(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_dns_client_identity_one_san() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(vec![SanType::DnsName(dns_id.into())]);
        let id = Id::parse_dns_name(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_dns_client_identity_many_sans() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(dns_id.into()),
            SanType::DnsName(bar_dns_id.into()),
            SanType::DnsName(nar_dns_id.into()),
            SanType::URI(spiffe_id.into()),
        ]);
        let id = Id::parse_dns_name(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn extracts_dns_client_id_when_others_not_uri_or_dns() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let email_san_1 = "foo@foo.com";
        let email_san_2 = "bar@bar.com";

        let cert = generate_cert_with_names(vec![
            SanType::DnsName(dns_id.into()),
            SanType::Rfc822Name(email_san_1.into()),
            SanType::Rfc822Name(email_san_2.into()),
        ]);
        let id = Id::parse_dns_name(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn extracts_spiffe_client_id_when_others_not_uri_or_dns() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";
        let email_san_1 = "foo@foo.com";
        let email_san_2 = "bar@bar.com";

        let cert = generate_cert_with_names(vec![
            SanType::URI(spiffe_id.into()),
            SanType::Rfc822Name(email_san_1.into()),
            SanType::Rfc822Name(email_san_2.into()),
        ]);
        let id = Id::parse_uri(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn skips_dns_san_with_trailing_dot() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local.";

        let cert = generate_cert_with_names(vec![SanType::DnsName(dns_id.into())]);
        let client_id = client_identity(&cert);
        assert_eq!(client_id, None);
    }
}
