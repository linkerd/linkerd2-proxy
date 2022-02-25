//! Unsafe code for accessing system-level counters for memory & CPU usage.

#![deny(
    warnings,
    rust_2018_idioms,
    rust_2018_idioms,
    clippy::disallowed_methods,
    unsafe_code
)]

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use self::linux::{blocking_stat, max_fds, ms_per_tick, open_fds, page_size, Stat};

#[cfg(not(target_os = "linux"))]
compile_error!("The system crate requires Linux");
