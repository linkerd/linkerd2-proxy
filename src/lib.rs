#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

mod addr;
pub mod app;
pub mod control;
mod dns;
mod identity;
pub mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
pub mod transport;

use self::addr::{Addr, NameAddr};
use linkerd2_conditional::Conditional;

pub use self::logging::trace;
pub use self::transport::SoOriginalDst;
