#![cfg(feature = "boring")]
#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod util;

use linkerd_meshtls::Mode;

#[test]
fn can_construct_store_der_roots() {
    util::can_construct_store(Mode::Boring, util::RootsFormat::Der);
}

#[test]
fn can_construct_store_pem_roots() {
    util::can_construct_store(Mode::Boring, util::RootsFormat::Pem);
}

#[test]
fn fails_processing_cert_when_wrong_id_configured_der_roots() {
    util::fails_processing_cert_when_wrong_id_configured(Mode::Boring, util::RootsFormat::Der);
}

#[test]
fn fails_processing_cert_when_wrong_id_configured_pem_roots() {
    util::fails_processing_cert_when_wrong_id_configured(Mode::Boring, util::RootsFormat::Pem);
}

#[test]
fn fails_to_construct_store_for_empty_der_roots() {
    util::fails_to_construct_store_for_empty_roots(Mode::Boring, util::RootsFormat::Der);
}

#[test]
fn fails_to_construct_store_for_empty_pem_roots() {
    util::fails_to_construct_store_for_empty_roots(Mode::Boring, util::RootsFormat::Pem);
}

#[tokio::test(flavor = "current_thread")]
async fn plaintext() {
    util::plaintext(Mode::Boring).await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_works() {
    util::proxy_to_proxy_tls_works(Mode::Boring).await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    util::proxy_to_proxy_tls_pass_through_when_identity_does_not_match(Mode::Boring).await;
}
