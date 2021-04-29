//! Shared infrastructure for integration tests

#![deny(warnings, rust_2018_idioms)]

pub use futures::{future, FutureExt, TryFuture, TryFutureExt};

pub use futures::stream::{Stream, StreamExt};
pub use linkerd_app_core::{self as app_core, Addr, Error};
pub use std::net::SocketAddr;
use std::{borrow::Cow, fmt, panic::Location};
use thiserror::Error;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub use tokio::sync::oneshot;
pub use tower::Service;
pub use tracing::*;
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

#[derive(Error)]
#[error("{}: {} ({}:{})", self.context, self.source, self.at.file(), self.at.line())]
pub struct ContextError {
    context: Cow<'static, str>,
    source: Error,
    at: &'static Location<'static>,
}

// === impl ContextError ===

impl ContextError {
    #[track_caller]
    pub(crate) fn ctx<E: Into<Error>>(context: impl Into<Cow<'static, str>>) -> impl Fn(E) -> Self {
        let context = context.into();
        let at = Location::caller();
        move |error| {
            let source = error.into();
            tracing::error!(%source, message = %context);
            Self {
                context: context.clone(),
                source,
                at,
            }
        }
    }
}

impl fmt::Debug for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `unwrap` and `expect` panic messages always use `fmt::Debug`, so in
        // order to get nicely formatted errors in panics, override our `Debug`
        // impl to use `Display`.
        fmt::Display::fmt(self, f)
    }
}
