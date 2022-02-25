#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

pub mod resolve;

pub use self::resolve::{Resolve, ResolveService, Update};
