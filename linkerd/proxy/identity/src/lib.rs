#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod certify;
pub mod metrics;
mod token;

pub use self::{
    certify::{AwaitCrt, Csr, LocalCrtKey},
    token::TokenSource,
};
pub use linkerd_identity::*;
pub use linkerd_tls_rustls::*;
