#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod util;

#[test]
fn fails_processing_cert_when_wrong_id_configured() {
    util::fails_processing_cert_when_wrong_id_configured();
}

#[tokio::test(flavor = "current_thread")]
async fn plaintext() {
    util::plaintext().await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_works() {
    util::proxy_to_proxy_tls_works().await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    util::proxy_to_proxy_tls_pass_through_when_identity_does_not_match().await;
}
