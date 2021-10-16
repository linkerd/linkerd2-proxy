#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod certify;
pub mod metrics;

pub use self::certify::{AwaitCrt, CrtKeySender, LocalCrtKey};
pub use linkerd_identity::*;
pub use linkerd_tls_rustls::*;
