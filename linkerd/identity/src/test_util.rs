use super::*;
use std::time::{Duration, SystemTime};

pub struct Identity {
    pub name: &'static str,
    pub trust_anchors: &'static [u8],
    pub crt: &'static [u8],
    pub key: &'static [u8],
}

pub static FOO_NS1: Identity = Identity {
    name: "foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca1.pem"),
    crt: include_bytes!("testdata/foo-ns1-ca1/crt.der"),
    key: include_bytes!("testdata/foo-ns1-ca1/key.p8"),
};

pub static BAR_NS1: Identity = Identity {
    name: "bar.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca1.pem"),
    crt: include_bytes!("testdata/bar-ns1-ca1/crt.der"),
    key: include_bytes!("testdata/bar-ns1-ca1/key.p8"),
};

impl Identity {
    pub fn trust_anchors(&self) -> TrustAnchors {
        let pem = ::std::str::from_utf8(self.trust_anchors).expect("utf-8");
        TrustAnchors::from_pem(pem).unwrap_or_else(TrustAnchors::empty)
    }

    pub fn key(&self) -> Key {
        Key::from_pkcs8(self.key).expect("key must be valid")
    }

    pub fn crt(&self) -> Crt {
        const HOUR: Duration = Duration::from_secs(60 * 60);

        let n = Name::from_str(self.name).expect("name must be valid");
        let der = self.crt.iter().copied().collect();
        Crt::new(LocalId(n), der, vec![], SystemTime::now() + HOUR)
    }

    pub fn validate(&self) -> Result<CrtKey, InvalidCrt> {
        let k = self.key();
        let c = self.crt();
        self.trust_anchors().certify(k, c)
    }
}
