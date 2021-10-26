use super::test_util::*;

#[test]
fn can_construct_client_and_server_config_from_valid_settings() {
    FOO_NS1.validate().expect("foo.ns1 must be valid");
}

#[test]
fn recognize_ca_did_not_issue_cert() {
    let s = Identity {
        trust_anchors: include_bytes!("testdata/ca2.pem"),
        ..FOO_NS1
    };
    assert!(s.validate().is_err(), "ca2 should not validate foo.ns1");
}

#[test]
fn recognize_cert_is_not_valid_for_identity() {
    let s = Identity {
        crt: BAR_NS1.crt,
        key: BAR_NS1.key,
        ..FOO_NS1
    };
    assert!(s.validate().is_err(), "identity should not be valid");
}

#[test]
#[ignore] // XXX this doesn't fail because we don't actually check the key against the cert...
fn recognize_private_key_is_not_valid_for_cert() {
    let s = Identity {
        key: BAR_NS1.key,
        ..FOO_NS1
    };
    assert!(s.validate().is_err(), "identity should not be valid");
}
