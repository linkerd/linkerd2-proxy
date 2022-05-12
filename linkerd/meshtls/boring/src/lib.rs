#![deny(
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

//! This crate provides an implementation of _meshtls_ backed by `boringssl` (as
//! provided by <https://github.com/cloudflare/boring>).
//!
//! There are several caveats with the current implementation:
//!
//! In its current form, this crate is compatible with the `meshtls-rustls`
//! implementation, which requires of ECDSA-P256-SHA256 keys & signature
//! algorithms. This crate doesn't actually constrain the algorithms beyond the
//! Mozilla's 'intermediate' (v5) [defaults][defaults]. But, the goal for
//! supporting `boring` is to provide a FIPS 140-2 compliant mode. There's a
//! [PR][fips-pr] that implements this, but code changes will likely be required
//! to enable this once it's merged/released.
//!
//! A new SSL context is created for each connection. This is probably
//! unnecessary, but it's simpler for now. We can revisit this if needed.
//!
//! This module is not enabled by default. See the `linkerd-meshtls` and
//! `linkerd2-proxy` crates for more information.
//!
//! [defaults]: https://wiki.mozilla.org/Security/Server_Side_TLS
//! [fips-pr]: https://github.com/cloudflare/boring/pull/52

mod client;
pub mod creds;
mod server;
#[cfg(test)]
mod tests;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};

fn fingerprint(c: &boring::x509::X509Ref) -> Option<String> {
    let digest = c.digest(boring::hash::MessageDigest::sha256()).ok()?;
    Some(hex::encode(digest)[0..8].to_string())
}
