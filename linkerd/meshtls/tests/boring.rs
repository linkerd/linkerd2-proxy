#![cfg(feature = "boring")]

mod util;

use linkerd_meshtls::Mode;

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
