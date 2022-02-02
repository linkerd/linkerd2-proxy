#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

mod credentials;
mod local;
mod name;

pub use self::{
    credentials::{Credentials, DerX509},
    local::LocalId,
    name::Name,
};
pub use linkerd_dns_name::InvalidName;
