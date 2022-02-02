#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

pub mod recover;

pub use self::recover::Recover;
pub use std::convert::Infallible;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn is_error<E: std::error::Error + 'static>(e: &(dyn std::error::Error + 'static)) -> bool {
    e.is::<E>() || e.source().map(is_error::<E>).unwrap_or(false)
}
