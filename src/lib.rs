#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]
#![cfg_attr(feature = "cargo-clippy", allow(new_without_default_derive))]
#![deny(warnings)]

#![recursion_limit="128"]

extern crate bytes;
extern crate env_logger;
extern crate linkerd2_fs_watch as fs_watch;
#[macro_use]
extern crate futures;
extern crate futures_mpsc_lossy;
extern crate futures_watch;
extern crate h2;
extern crate http;
extern crate httparse;
extern crate hyper;
#[cfg(target_os = "linux")]
extern crate inotify;
extern crate ipnet;
#[cfg(target_os = "linux")]
extern crate libc;
#[macro_use]
extern crate log;
#[cfg_attr(test, macro_use)]
extern crate indexmap;
#[cfg(target_os = "linux")]
extern crate procinfo;
extern crate prost;
extern crate prost_types;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
extern crate rand;
extern crate regex;
extern crate ring;
extern crate tokio;
extern crate tokio_timer;
extern crate tower_grpc;
extern crate tower_h2;
extern crate tower_http;
extern crate tower_util;
extern crate trust_dns_resolver;
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
pub mod ctx;
mod dns;
mod drain;
mod logging;
mod proxy;
mod svc;
mod tap;
pub mod telemetry;
pub mod transport;

use self::addr::{Addr, NameAddr};
use self::conditional::Conditional;
pub use self::transport::SoOriginalDst;
