#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(clippy::large_enum_variant)]
#![forbid(unsafe_code)]

//! This crate provides a static interface for the proxy's x509 certificate
//! provisioning and creation of client/server services. It supports the
//! `boring` and `rustls` TLS backends.
//!
//! This crate may be compiled without either implementation, in which case it
//! will fail at runtime.  This enables an implementation to be chosen by the
//! proxy's frontend, so that other crates can depend on this crate without
//! having to pin a TLS implementation. Furthermore, this crate supports both
//! backends simultaneously so it can be compiled with `--all-features`.

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{Server, ServerIo, TerminateFuture},
};
use linkerd_dns_name as dns;
use linkerd_error::Result;
use linkerd_identity as id;

pub use linkerd_meshtls_rustls as rustls;
pub use rustls::creds::watch;

#[derive(Default, Copy, Clone, Debug)]
pub struct Mode;

// === impl Mode ===

impl Mode {
    pub fn watch(
        self,
        local_id: id::Id,
        server_name: dns::Name,
        roots_pem: &str,
    ) -> Result<(creds::Store, creds::Receiver)> {
        watch(local_id, server_name, roots_pem)
    }
}
