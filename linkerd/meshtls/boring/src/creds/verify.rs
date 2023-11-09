use boring::x509::X509;
use linkerd_dns_name as dns;
use linkerd_identity as id;
use std::io;

pub(crate) fn verify_id(cert: &X509, id: &id::Id) -> io::Result<()> {
    for san in cert.subject_alt_names().into_iter().flatten() {
        if let Some(n) = san.dnsname() {
            if let Ok(name) = n.parse::<dns::Name>() {
                if *name == id.to_str() {
                    return Ok(());
                }
            }
            tracing::warn!("DNS SAN name {} could not be parsed", n)
        }
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "certificate does not match TLS identity",
    ))
}

#[cfg(test)]
mod tests {
    use super::verify_id;
    use boring::x509::X509;
    use linkerd_identity as id;
    use rcgen::generate_simple_self_signed;
    use std::str::FromStr;

    fn generate_cert_with_name(name: Option<&str>) -> X509 {
        let sans = name.map(|s| vec![s.into()]).unwrap_or_default();

        let cert_data = generate_simple_self_signed(sans)
            .expect("should generate cert")
            .serialize_der()
            .expect("should serialize");

        X509::from_der(cert_data.as_slice()).expect("should parse")
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
