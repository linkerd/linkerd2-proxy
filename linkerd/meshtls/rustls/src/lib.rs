#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
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
