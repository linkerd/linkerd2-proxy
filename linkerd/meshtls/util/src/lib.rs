use linkerd_error::Result;
use linkerd_identity::Id;
use std::io;
use std::str::FromStr;

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
                let name = match n {
                    GeneralName::DNSName(dns) => Some(dns),
                    GeneralName::URI(uri) => Some(uri),
                    _ => None,
                }?;

                if *name == "*" {
                    // Wildcards can perhaps be handled in a future path...
                    return None;
                }

                match Id::from_str(name) {
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
    // TODO: we need to ensure that we follow SVID verification
    // requirementes for x509 certs outlined in:
    // https://github.com/spiffe/spiffe/blob/main/standards/X509-SVID.md#the-x509-spiffe-verifiable-identity-document
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
    use crate::{client_identity, verify_id, Id};
    use rcgen::generate_simple_self_signed;
    use std::str::FromStr;

    fn generate_cert_with_names(names: Vec<&str>) -> Vec<u8> {
        let sans: Vec<String> = names.into_iter().map(|s| s.into()).collect();

        generate_simple_self_signed(sans)
            .expect("should generate cert")
            .serialize_der()
            .expect("should serialize")
    }

    #[test]
    fn cert_with_dns_san_matches_dns_id() {
        let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![dns_name]);
        let id = Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_spiffe_san_matches_spiffe_id() {
        let spiffe_uri = "spiffe://identity.linkerd.cluster.local/ns/ns1/sa/foo";
        let cert = generate_cert_with_names(vec![spiffe_uri]);
        let id = Id::from_str(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_san_does_not_match_dns_id() {
        let dns_name_cert = vec!["foo.ns1.serviceaccount.identity.linkerd.cluster.local"];
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(dns_name_cert);
        let id = Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_spiffe_san_does_not_match_spiffe_id() {
        let spiffe_uri_cert = vec!["spiffe://identity.linkerd.cluster.local/ns/ns1/sa/foo"];
        let spiffe_uri = "spiffe://identity.linkerd.cluster.local/ns/ns1/sa/bar";

        let cert = generate_cert_with_names(spiffe_uri_cert);
        let id = Id::from_str(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_spiffe_san_does_not_match_dns_id() {
        let uri_cert = vec!["spiffe://some-trust-comain/some-system/some-component"];
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(uri_cert);
        let id = Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_dns_san_does_not_match_spiffe_id() {
        let dns_name_cert = vec!["bar.ns1.serviceaccount.identity.linkerd.cluster.local"];
        let spiffe_uri = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(dns_name_cert);
        let id = Id::from_str(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_dns_id() {
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![]);
        let id = Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_no_san_does_not_verify_for_spiffe_id() {
        let spiffe_uri = "spiffe://some-trust-comain/some-system/some-component";
        let cert = generate_cert_with_names(vec![]);
        let id = Id::from_str(spiffe_uri).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_multiple_sans_one_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
        let id = Id::from_str(foo_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_multiple_sans_one_matches_spiffe_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
        let id = Id::from_str(spiffe_id).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    fn cert_with_dns_multiple_sans_none_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
        let id = Id::from_str(nar_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn cert_with_multiple_sans_none_matches_spiffe_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, nar_dns_id]);
        let id = Id::from_str(spiffe_id).expect("should parse SPIFFE id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    fn can_extract_spiffe_client_identity_one_san() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![spiffe_id]);
        let id = Id::from_str(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_spiffe_client_identity_many_sans() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(vec![spiffe_id, bar_dns_id, nar_dns_id, spiffe_id]);
        let id = Id::from_str(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_dns_client_identity_one_san() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(vec![dns_id]);
        let id = Id::from_str(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn can_extract_dns_client_identity_many_sans() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![dns_id, bar_dns_id, nar_dns_id, spiffe_id]);
        let id = Id::from_str(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn extracts_dns_client_id_when_others_not_uri_or_dns() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let email_san_1 = "foo@foo.com";
        let email_san_2 = "bar@bar.com";

        let cert = generate_cert_with_names(vec![dns_id, email_san_1, email_san_2]);
        let id = Id::from_str(dns_id).expect("should parse DNS id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn extracts_spiffe_client_id_when_others_not_uri_or_dns() {
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";
        let email_san_1 = "foo@foo.com";
        let email_san_2 = "bar@bar.com";

        let cert = generate_cert_with_names(vec![spiffe_id, email_san_1, email_san_2]);
        let id = Id::from_str(spiffe_id).expect("should parse SPIFFE id");
        let client_id = client_identity(&cert);
        assert_eq!(client_id, Some(id));
    }

    #[test]
    fn skips_uri_san_with_non_spiffe_scheme() {
        let spiffe_id = "https://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![spiffe_id]);
        let client_id = client_identity(&cert);
        assert_eq!(client_id, None);
    }

    #[test]
    fn skips_dns_san_with_trailing_dor() {
        let dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local.";

        let cert = generate_cert_with_names(vec![dns_id]);
        let client_id = client_identity(&cert);
        assert_eq!(client_id, None);
    }
}
