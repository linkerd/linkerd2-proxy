#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]

use linkerd2_addr::{self as addr, Addr, NameAddr};
use linkerd2_conditional::Conditional;
use linkerd2_drain as drain;
use linkerd2_error::{Error, Never};
use linkerd2_identity as identity;
use linkerd2_metrics as metrics;
use linkerd2_opencensus as opencensus;
use linkerd2_proxy_api as api;
use linkerd2_proxy_api_resolve as api_resolve;
use linkerd2_proxy_core as core;
use linkerd2_task as task;
use linkerd2_trace_context as trace_context;

pub mod app;
mod dns;
pub mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
mod transport;

pub use self::logging::trace;
pub use self::transport::SoOriginalDst;
