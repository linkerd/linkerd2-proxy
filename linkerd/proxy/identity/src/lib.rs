#![deny(warnings, rust_2018_idioms)]

pub mod certify;
pub mod metrics;

pub use self::certify::{AwaitCrt, CrtKeySender, Local};
// XXX pub use linkerd_identity::{Crt, CrtKey, Csr, InvalidName, Key, Name, TokenSource, TrustAnchors};
