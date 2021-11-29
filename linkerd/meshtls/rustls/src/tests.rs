use linkerd_identity::{Credentials, DerX509};
use linkerd_tls_test_util::*;
use std::time::Duration;

fn load(ent: &Entity) -> crate::creds::Store {
    let roots_pem = std::str::from_utf8(ent.trust_anchors).expect("valid PEM");
    let (store, _) = crate::creds::watch(
        ent.name.parse().unwrap(),
        roots_pem,
        ent.key,
        b"fake CSR data",
    )
    .expect("credentials must be readable");
    store
}

#[test]
fn can_construct_client_and_server_config_from_valid_settings() {
    assert!(load(&FOO_NS1)
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            std::time::SystemTime::now() + Duration::from_secs(600)
        )
        .is_ok());
}

#[test]
fn recognize_ca_did_not_issue_cert() {
    assert!(load(&FOO_NS1_CA2)
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            std::time::SystemTime::now() + Duration::from_secs(600)
        )
        .is_err());
}

#[test]
fn recognize_cert_is_not_valid_for_identity() {
    assert!(load(&BAR_NS1)
        .set_certificate(
            DerX509(FOO_NS1.crt.to_vec()),
            vec![],
            std::time::SystemTime::now() + Duration::from_secs(600)
        )
        .is_err());
}
