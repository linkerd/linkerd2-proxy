// #![deny(warnings)]
#![recursion_limit = "128"]

extern crate bytes;
#[macro_use]
extern crate futures;
extern crate futures_mpsc_lossy;
extern crate futures_watch;
extern crate h2;
extern crate http;
extern crate http_body;
extern crate httparse;
extern crate hyper;
extern crate indexmap;
extern crate ipnet;
#[cfg(target_os = "linux")]
extern crate libc;
extern crate log;
#[cfg(target_os = "linux")]
extern crate procinfo;
extern crate prost;
extern crate prost_types;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
extern crate rand;
extern crate regex;
extern crate tokio;
extern crate tokio_timer;
#[macro_use]
extern crate tracing;
extern crate tower;
extern crate tower_grpc;
extern crate tower_util;
extern crate tracing_fmt;
extern crate tracing_futures;
extern crate try_lock;

#[macro_use]
extern crate linkerd2_metrics;
extern crate linkerd2_never as never;
extern crate linkerd2_proxy_api as api;
extern crate linkerd2_task as task;
extern crate linkerd2_timeout as timeout;

// `linkerd2_metrics` is needed to satisfy the macro, but this is nicer to use internally.
use self::linkerd2_metrics as metrics;

mod addr;
pub mod app;
mod conditional;
pub mod control;
pub mod convert;
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
