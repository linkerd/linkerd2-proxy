#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

mod server;
mod stack;

pub use self::server::{Admin, Latch, Readiness};
pub use self::stack::{Config, Task};
