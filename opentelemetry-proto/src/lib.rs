//! gRPC bindings for OpenTelemetry.
//!
//! Vendored from <https://github.com/open-telemetry/opentelemetry-proto/>.

// proto mod contains file generated by protobuf or other build tools.
// we shouldn't manually change it. Thus skip format and lint check.
#[rustfmt::skip]
#[allow(warnings)]
pub mod proto;

pub mod transform;
