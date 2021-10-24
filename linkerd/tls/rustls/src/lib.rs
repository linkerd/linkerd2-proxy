#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(dead_code)]

mod client;
mod creds;
mod server;
#[cfg(feature = "test-util")]
pub mod test_util;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture},
    creds::Creds,
    server::{terminate, ServerIo, TerminateFuture},
};
pub use tokio_rustls::rustls::*;
