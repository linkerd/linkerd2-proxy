#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod util;

#[test]
fn fails_processing_cert_when_wrong_id_configured() {
    linkerd_rustls::install_default_provider();
    util::fails_processing_cert_when_wrong_id_configured();
}

#[tokio::test(flavor = "current_thread")]
async fn plaintext() {
    linkerd_rustls::install_default_provider();
    util::plaintext().await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_works() {
    linkerd_rustls::install_default_provider();
    util::proxy_to_proxy_tls_works().await;
}

#[tokio::test(flavor = "current_thread")]
async fn proxy_to_proxy_tls_pass_through_when_identity_does_not_match() {
    linkerd_rustls::install_default_provider();
    util::proxy_to_proxy_tls_pass_through_when_identity_does_not_match().await;
}
