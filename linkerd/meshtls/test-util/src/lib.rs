use linkerd_identity as id;
use rcgen::generate_simple_self_signed;
use std::io::Result;
use std::str::FromStr;

fn generate_cert_with_names<C>(names: Vec<&str>, vec_to_cert: impl FnOnce(Vec<u8>) -> C) -> C {
    let sans: Vec<String> = names.into_iter().map(|s| s.into()).collect();

    let cert_data = generate_simple_self_signed(sans)
        .expect("should generate cert")
        .serialize_der()
        .expect("should serialize");

    vec_to_cert(cert_data)
}

pub fn cert_with_dns_san_matches_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let dns_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let cert = generate_cert_with_names(vec![dns_name], vec_to_cert);
    let id = id::Id::from_str(dns_name).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_ok());
}

pub fn cert_with_dns_san_does_not_match_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let dns_name_cert = vec!["foo.ns1.serviceaccount.identity.linkerd.cluster.local"];
    let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

    let cert = generate_cert_with_names(dns_name_cert, vec_to_cert);
    let id = id::Id::from_str(dns_name).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_err());
}

pub fn cert_with_uri_san_does_not_match_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let uri_name_cert = vec!["spiffe://some-trust-comain/some-system/some-component"];
    let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";

    let cert = generate_cert_with_names(uri_name_cert, vec_to_cert);
    let id = id::Id::from_str(dns_name).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_err());
}

pub fn cert_with_no_san_does_not_verify_for_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let dns_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let cert = generate_cert_with_names(vec![], vec_to_cert);
    let id = id::Id::from_str(dns_name).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_err());
}

pub fn cert_with_dns_multiple_sans_one_matches_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

    let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id], vec_to_cert);
    let id = id::Id::from_str(foo_dns_id).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_ok());
}

pub fn cert_with_dns_multiple_sans_none_matches_dns_id<C>(
    verify_id: impl FnOnce(&C, &id::Id) -> Result<()>,
    vec_to_cert: impl FnOnce(Vec<u8>) -> C,
) {
    let foo_dns_id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let bar_dns_id = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let nar_dns_id = "nar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let spiffe_id = "spiffe://some-trust-comain/some-system/some-component";

    let cert = generate_cert_with_names(vec![foo_dns_id, bar_dns_id, spiffe_id], vec_to_cert);
    let id = id::Id::from_str(nar_dns_id).expect("should parse DNS id");
    assert!(verify_id(&cert, &id).is_err());
}
