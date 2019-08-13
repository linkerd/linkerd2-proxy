#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

pub mod app;
pub mod control;
mod dns;
pub mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
pub mod transport;

use linkerd2_addr::{Addr, NameAddr};
use linkerd2_conditional::Conditional;
use linkerd2_identity as identity;

pub use self::logging::trace;
pub use self::transport::SoOriginalDst;
