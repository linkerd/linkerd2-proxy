use super::*;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use std::{fs, io};
use Conditional;

pub struct Strings {
    pub identity: &'static str,
    pub trust_anchors: &'static str,
    pub crt: &'static str,
    pub key: &'static str,
    pub csr: &'static str,
}

pub static FOO_NS1: Strings = Strings {
    identity: "foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: "ca1.pem",
    crt: "foo-ns1-ca1.crt",
    key: "foo-ns1-ca1.p8",
    csr: "foo-ns1.csr",
};

pub static BAR_NS1: Strings = Strings {
    identity: "bar.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: "ca1.pem",
    crt: "bar-ns1-ca1.crt",
    key: "bar-ns1-ca1.p8",
    csr: "bar-ns1.csr",
};

impl Strings {
    pub fn trust_anchors(&self) -> TrustAnchors {
        let pem = fs::read(&self.trust_anchors).expect("failed to read trust anchors");
        let pem = ::std::str::from_utf8(&pem).expect("utf-8");
        TrustAnchors::from_pem(pem).unwrap_or_else(|| TrustAnchors::empty())
    }

    pub fn key(&self) -> Key {
        let p8 = fs::read(&self.key).expect("key must be valid");
        Key::from_pkcs8(&p8).expect("key must be valid")
    }

    pub fn crt(&self) -> Crt {
        let n = Name::from_sni_hostname(self.identity.as_bytes()).expect("name must be valid");
        let pem = fs::read(&self.crt).expect("crt must be valid");
        let mut crts = rustls::internal::pemfile::certs(&mut io::Cursor::new(pem))
            .expect("crt must be valid")
            .into_iter()
            .map(|c| c.as_ref().to_vec());
        let c = crts.next().expect("must have at least one crt");
        const HOUR: Duration = Duration::from_secs(60 * 60);
        Crt::new(n, c, crts.collect(), SystemTime::now() + HOUR)
    }

    pub fn validate(&self) -> Result<CrtKey, InvalidCrt> {
        let k = self.key();
        let c = self.crt();
        self.trust_anchors().certify(k, c)
    }
}
