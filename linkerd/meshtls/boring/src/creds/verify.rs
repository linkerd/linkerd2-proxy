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
    use boring::x509::X509;

    fn vec_to_cert(data: Vec<u8>) -> X509 {
        X509::from_der(data.as_slice()).expect("should parse")
    }

    linkerd_meshtls::generate_verify_id_tests!(X509, vec_to_cert);
}
