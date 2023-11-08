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
        }
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        "certificate does not match tls id",
    ))
}
