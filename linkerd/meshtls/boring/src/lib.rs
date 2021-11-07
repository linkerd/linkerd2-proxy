#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

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
