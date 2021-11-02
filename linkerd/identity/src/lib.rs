#![deny(warnings, rust_2018_idioms)]
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
