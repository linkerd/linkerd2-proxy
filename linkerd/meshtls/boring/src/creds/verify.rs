use boring::x509::X509;
use linkerd_dns_name as dns;
use linkerd_identity as id;
use std::io;

pub(crate) fn verify_id(cert: &X509, id: &id::Id) -> io::Result<()> {
    for san in cert.subject_alt_names().into_iter().flatten() {
        if let Some(n) = san.dnsname() {
            match n.parse::<dns::Name>() {
                Ok(name) if *name == id.to_str() => return Ok(()),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "DNS SAN name {} could not be parsed", n),
            }
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
    use linkerd_meshtls_test_util as test_util;

    fn vec_to_cert(data: Vec<u8>) -> X509 {
        X509::from_der(data.as_slice()).expect("should parse")
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
