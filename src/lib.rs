#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

mod addr;
pub mod app;
mod conditional;
pub mod control;
mod dns;
mod drain;
mod identity;
pub mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
pub mod transport;

use self::addr::{Addr, NameAddr};
use self::conditional::Conditional;

pub use self::logging::trace;
pub use self::transport::SoOriginalDst;
