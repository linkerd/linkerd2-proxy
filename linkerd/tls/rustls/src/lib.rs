#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(dead_code)]

mod client;
pub mod creds;
mod server;

pub use self::{
    client::{ClientIo, Connect, ConnectFuture, NewClient},
    server::{terminate, ServerIo, Terminate, TerminateFuture},
};
pub use tokio_rustls::rustls::*;
