#![deny(warnings, rust_2018_idioms)]

pub use linkerd_identity::LocalId;
pub use rustls::TLSError as Error;

pub mod client;
pub mod server;

pub use self::{
    client::{Client, ConditionalServerId, NoServerId, ServerId},
    server::{ClientId, NewDetectTls},
};
