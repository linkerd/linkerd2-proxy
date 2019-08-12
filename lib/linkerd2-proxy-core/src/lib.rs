#![deny(warnings, rust_2018_idioms)]

/// A dynamic error type.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
