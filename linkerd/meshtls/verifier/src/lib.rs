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
                let dns_san = match n {
                    GeneralName::DNSName(dns) => Some(dns),
                    _ => None,
                }?;

                dns_san.parse::<Id>().ok()
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
    // TODO: we need to make sure that we handle SPIFFE IDs as well
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
    use crate::verify_id;
    use linkerd_identity as id;
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
    pub fn cert_with_dns_san_matches_dns_id() {
        let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![dns_name]);
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    pub fn cert_with_dns_san_does_not_match_dns_id() {
        let dns_name_cert = vec!["foo.ns1.serviceaccount.identity.linkerd.cluster.local"];
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(dns_name_cert);
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    pub fn cert_with_uri_san_does_not_match_dns_id() {
        let uri_name_cert = vec!["spiffe://some-trust-comain/some-system/some-component"];
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

        let cert = generate_cert_with_names(uri_name_cert);
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    pub fn cert_with_no_san_does_not_verify_for_dns_id() {
        let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let cert = generate_cert_with_names(vec![]);
        let id = id::Id::from_str(dns_name).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }

    #[test]
    pub fn cert_with_dns_multiple_sans_one_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
        let id = id::Id::from_str(foo_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_ok());
    }

    #[test]
    pub fn cert_with_dns_multiple_sans_none_matches_dns_id() {
        let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
        let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
        let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

        let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id]);
        let id = id::Id::from_str(nar_dns_id).expect("should parse DNS id");
        assert!(verify_id(&cert, &id).is_err());
    }
}
