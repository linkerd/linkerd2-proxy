#![deny(warnings, rust_2018_idioms)]

mod never;
pub mod recover;

pub use self::never::Never;
pub use self::recover::Recover;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub type Result<T, E = Error> = std::result::Result<T, E>;
