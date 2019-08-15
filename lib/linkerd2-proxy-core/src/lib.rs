#![deny(warnings, rust_2018_idioms)]

pub use linkerd2_drain as drain;

pub mod listen;
pub mod resolve;

pub use self::resolve::{Resolution, Resolve};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
