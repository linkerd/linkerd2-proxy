#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

use linkerd2_addr::{self as addr, Addr, NameAddr};
use linkerd2_conditional::Conditional;
use linkerd2_identity as identity;
use linkerd2_metrics as metrics;
use linkerd2_never::Never;
use linkerd2_proxy_api as api;
use linkerd2_proxy_core::{self as core, drain, Error};
use linkerd2_task as task;

pub mod app;
pub mod control;
mod dns;
pub mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
pub mod transport;

pub use self::logging::trace;
pub use self::transport::SoOriginalDst;
