//! Shared infrastructure for integration tests

#![deny(warnings, rust_2018_idioms)]

pub use futures::{future, FutureExt, TryFuture, TryFutureExt};

pub use futures::stream::{Stream, StreamExt};
pub use linkerd_app_core::{self as app_core, Addr, Error};
pub use std::net::SocketAddr;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub use tokio::sync::oneshot;
pub use tower::Service;
pub use tracing::*;
pub use tracing_subscriber::prelude::*;

pub mod io {
    pub use tokio::io::*;
    pub use tokio_test::io::*;
}

pub mod connect;
pub mod http_util;
pub mod profile;
pub mod resolver;
pub mod service;
pub mod track;
pub mod transport;

pub fn resolver<E>() -> resolver::Dst<E> {
    resolver::Resolver::default()
}

pub fn profiles() -> resolver::Profiles {
    profile::resolver()
}

pub fn connect<E>() -> connect::Connect<E> {
    connect::Connect::default()
}

pub fn io() -> io::Builder {
    io::Builder::new()
}

/// By default, disable logging in modules that are expected to error in tests.
const DEFAULT_LOG: &str = "warn,\
                           linkerd=debug,\
                           linkerd_proxy_http=error,\
                           linkerd_proxy_transport=error";

pub fn trace_subscriber() -> (Dispatch, app_core::trace::Handle) {
    use std::env;
    let log_level = env::var("LINKERD2_PROXY_LOG")
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| DEFAULT_LOG.to_string());
    let log_format = env::var("LINKERD2_PROXY_LOG_FORMAT").unwrap_or_else(|_| "PLAIN".to_string());
    env::set_var("LINKERD2_PROXY_LOG_FORMAT", &log_format);
    // This may fail, since the global log compat layer may have been
    // initialized by another test.
    let _ = app_core::trace::init_log_compat();
    app_core::trace::Settings::default()
        .filter(log_level)
        .format(log_format)
        .test(true)
        .build()
}

pub fn trace_init() -> tracing::dispatcher::DefaultGuard {
    let (d, _) = trace_subscriber();
    tracing::dispatcher::set_default(&d)
}
