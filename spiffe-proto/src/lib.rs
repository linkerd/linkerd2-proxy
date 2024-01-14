//! gRPC bindings for SPIFFE workload api.
//!
//! Vendored from <https://github.com/spiffe/go-spiffe/blob/main/v2/proto/spiffe/workload/workload.proto>.

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(clippy::derive_partial_eq_without_eq)]
#![forbid(unsafe_code)]

pub mod client {
    include!("gen/spiffe.workloadapi.rs");
}
