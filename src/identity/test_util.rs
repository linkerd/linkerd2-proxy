extern crate webpki;

use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub struct Strings {
    pub name: &'static str,
    pub trust_anchors: &'static str,
    pub crt: &'static str,
    pub key: &'static str,
    //pub csr: &'static str,
}

pub static FOO_NS1: Strings = Strings {
    name: "foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: "ca1.pem",
    crt: "foo-ns1-ca1/crt.der",
    key: "foo-ns1-ca1/key.p8",
    //csr: "foo-ns1-ca1/csr.der",
};

pub static BAR_NS1: Strings = Strings {
    name: "bar.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: "ca1.pem",
    crt: "bar-ns1-ca1/crt.der",
    key: "bar-ns1-ca1/key.p8",
    //csr: "bar-ns1-ca1/csr.der",
};

impl Strings {
    fn read(n: &str) -> Vec<u8> {
        let dir = PathBuf::from("src/identity/testdata");
        let p = dir.join(n);
        match fs::read(&p) {
            Ok(b) => b,
            Err(e) => {
                panic!("Failed to read {}: {}", p.to_str().unwrap(), e);
            }
        }
    }

    pub fn trust_anchors(&self) -> TrustAnchors {
        let b = Self::read(&self.trust_anchors);
        let pem = ::std::str::from_utf8(&b).expect("utf-8");
        TrustAnchors::from_pem(pem).unwrap_or_else(|| TrustAnchors::empty())
    }

    pub fn key(&self) -> Key {
        let p8 = Self::read(&self.key);
        Key::from_pkcs8(&p8).expect("key must be valid")
    }

    pub fn crt(&self) -> Crt {
        const HOUR: Duration = Duration::from_secs(60 * 60);

        let n = Name::from_hostname(self.name.as_bytes()).expect("name must be valid");
        let der = Self::read(&self.crt);
        Crt::new(n, der, vec![], SystemTime::now() + HOUR)
    }

    pub fn validate(&self) -> Result<CrtKey, InvalidCrt> {
        let k = self.key();
        let c = self.crt();
        self.trust_anchors().certify(k, c)
    }
}
