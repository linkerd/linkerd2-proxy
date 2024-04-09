use linkerd_identity::{Credentials, DerX509, Roots};
use linkerd_tls_test_util::*;
use std::time::{Duration, SystemTime};

fn load(ent: &Entity) -> (crate::creds::Store, Roots) {
    let roots_pem = std::str::from_utf8(ent.trust_anchors).expect("valid PEM");
    let roots = Roots::Pem(roots_pem.into());
    let (store, _) = crate::creds::watch(
        ent.name.parse().unwrap(),
        ent.name.parse().unwrap(),
        roots.clone(),
    )
    .expect("credentials must be readable");
    (store, roots)
}

#[test]
fn can_construct_client_and_server_config_from_valid_settings() {
    let (mut store, roots) = load(&FOO_NS1);
    assert!(store
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            FOO_NS1.key.to_vec(),
            SystemTime::now() + Duration::from_secs(1000),
            roots
        )
        .is_ok());
}

#[test]
fn recognize_ca_did_not_issue_cert() {
    let (mut store, roots) = load(&FOO_NS1_CA2);
    assert!(store
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            FOO_NS1.key.to_vec(),
            SystemTime::now() + Duration::from_secs(1000),
            roots
        )
        .is_err());
}

#[test]
fn recognize_cert_is_not_valid_for_identity() {
    let (mut store, roots) = load(&BAR_NS1);
    assert!(store
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            FOO_NS1.key.to_vec(),
            SystemTime::now() + Duration::from_secs(1000),
            roots
        )
        .is_err());
}
