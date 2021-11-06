pub struct Entity {
    pub name: &'static str,
    pub trust_anchors: &'static [u8],
    pub crt: &'static [u8],
    pub key: &'static [u8],
}

pub static DEFAULT_DEFAULT: Entity = Entity {
    name: "default.default.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca1.pem"),
    crt: include_bytes!("testdata/default-default-ca1/crt.der"),
    key: include_bytes!("testdata/default-default-ca1/key.p8"),
};

pub static FOO_NS1: Entity = Entity {
    name: "foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca1.pem"),
    crt: include_bytes!("testdata/foo-ns1-ca1/crt.der"),
    key: include_bytes!("testdata/foo-ns1-ca1/key.p8"),
};

pub static FOO_NS1_CA2: Entity = Entity {
    name: "foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca2.pem"),
    crt: include_bytes!("testdata/foo-ns1-ca2/crt.der"),
    key: include_bytes!("testdata/foo-ns1-ca2/key.p8"),
};

pub static BAR_NS1: Entity = Entity {
    name: "bar.ns1.serviceaccount.identity.linkerd.cluster.local",
    trust_anchors: include_bytes!("testdata/ca1.pem"),
    crt: include_bytes!("testdata/bar-ns1-ca1/crt.der"),
    key: include_bytes!("testdata/bar-ns1-ca1/key.p8"),
};
