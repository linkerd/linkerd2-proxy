#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod credentials;
mod local;
mod name;
mod tls_name;

pub use self::{
    credentials::{Credentials, DerX509},
    local::LocalId,
    local::LocalName,
    name::Name,
    tls_name::TlsName,
};
pub use linkerd_dns_name::InvalidName;
