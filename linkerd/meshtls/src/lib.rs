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

mod backend;
mod client;
pub mod creds;
mod server;
#[cfg(test)]
mod tests;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    creds::watch,
    server::{Server, ServerIo, TerminateFuture},
};
