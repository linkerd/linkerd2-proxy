#![deny(warnings, rust_2018_idioms)]

pub mod certify;

pub use self::certify::{AwaitCrt, CrtKeySender, Local};
pub use linkerd2_identity::{Crt, CrtKey, Csr, InvalidName, Key, Name, TokenSource, TrustAnchors};
