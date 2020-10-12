//! Shared infrastructure for integration tests

#![deny(warnings, rust_2018_idioms)]

pub use futures::{future, FutureExt, TryFuture, TryFutureExt};

pub use linkerd2_app_core::{self as app_core, Addr, Error};
pub use std::net::SocketAddr;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub use tokio::stream::{Stream, StreamExt};
pub use tokio::sync::oneshot;
pub use tower::Service;
pub use tracing::*;
pub use tracing_subscriber::prelude::*;

use std::fmt;
pub use tokio_test::io;

pub mod connect;
pub mod resolver;

pub fn resolver<T, E>() -> resolver::DstResolver<T, E>
where
    T: std::hash::Hash + Eq + fmt::Debug,
{
    resolver::Resolver::new()
}

pub fn profiles<T>() -> resolver::ProfileResolver<T>
where
    T: std::hash::Hash + Eq + fmt::Debug,
{
    resolver::Resolver::new()
}

pub fn connect<E: fmt::Debug>() -> connect::Connect<E> {
    connect::Connect::new()
}

pub fn io() -> io::Builder {
    io::Builder::new()
}

/// By default, disable logging in modules that are expected to error in tests.
const DEFAULT_LOG: &'static str = "error,\
                                   linkerd2_proxy_http=off,\
                                   linkerd2_proxy_transport=off";

pub fn trace_subscriber() -> (Dispatch, app_core::trace::Handle) {
    use std::env;
    let log_level = env::var("LINKERD2_PROXY_LOG")
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| DEFAULT_LOG.to_owned());
    env::set_var("RUST_LOG", &log_level);
    env::set_var("LINKERD2_PROXY_LOG", &log_level);
    let log_format = env::var("LINKERD2_PROXY_LOG_FORMAT").unwrap_or_else(|_| "PLAIN".to_string());
    env::set_var("LINKERD2_PROXY_LOG_FORMAT", &log_format);
    // This may fail, since the global log compat layer may have been
    // initialized by another test.
    let _ = app_core::trace::init_log_compat();
    app_core::trace::with_filter_and_format(&log_level, &log_format)
}

pub fn trace_init() -> tracing::dispatcher::DefaultGuard {
    let (d, _) = trace_subscriber();
    tracing::dispatcher::set_default(&d)
}
