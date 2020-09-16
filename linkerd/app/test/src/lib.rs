//! Shared infrastructure for integration tests

#![deny(warnings, rust_2018_idioms)]

pub use futures::{future, FutureExt, TryFuture, TryFutureExt};
pub use linkerd2_app_core::{self as app_core, Addr, Error};
pub use std::{future::Future, net::SocketAddr, pin::Pin};
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub use tokio::stream::{Stream, StreamExt};
pub use tokio::sync::oneshot;
pub use tower::{make::MakeService, Service};
pub use tracing::*;
pub use tracing_subscriber::prelude::*;

pub mod connect;
pub mod identity;
pub mod io;
pub mod refine;
pub mod resolver;

pub fn resolver<T, E>() -> resolver::Resolver<T, E>
where
    T: std::hash::Hash + Eq + std::fmt::Debug,
{
    resolver::Resolver::new()
}

pub fn connect() -> connect::Connect {
    connect::Connect::new()
}

pub fn identity(dir: &'static str) -> identity::Identity {
    identity::Identity::new(dir)
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

pub trait TestMakeServiceExt<T, R>: MakeService<T, R> {
    /// Oneshots a `MakeService` implementation with the given target and then
    /// oneshots the returned `Service` with the given request, expecting both
    /// to succeed.
    ///
    /// # Panics
    ///
    /// - If the `MakeService` fails to become ready.
    /// - If making the `Service` fails.
    /// - If the returned `Service` fails to become ready.
    /// - If calling the returned `Service` fails.
    fn make_oneshot(
        self,
        target: T,
        req: R,
    ) -> Pin<Box<dyn Future<Output = Self::Response> + Send + 'static>>
    where
        Self: Sized;
}

impl<S, T, R> TestMakeServiceExt<T, R> for S
where
    S: MakeService<T, R> + Send + 'static,
    S::Service: Send,
    S::Future: Send,
    S::MakeError: Into<Error>,
    <S::Service as Service<R>>::Future: Send,
    <S::Service as Service<R>>::Error: Into<Error>,
    T: std::fmt::Debug + Send + 'static,
    R: std::fmt::Debug + Send + 'static,
{
    fn make_oneshot(
        mut self,
        target: T,
        req: R,
    ) -> Pin<Box<dyn Future<Output = Self::Response> + Send + 'static>>
    where
        Self: Sized,
    {
        use tower::ServiceExt;
        Box::pin(async move {
            // format the target eagerly, because this is a test, and we care
            // more about getting useful output than about not allocating a few strings.
            let target_str = format!("{:?}", target);

            // MakeService doesn't have its own `oneshot` method, so drive it to
            // readiness manually (rather than adding a bunch more bounds to
            // remind the compiler that the `MakeService` _is_ also a `Service`).
            futures::future::poll_fn(|cx| self.poll_ready(cx))
                .err_into()
                .await
                .unwrap_or_else(|error| {
                    tracing::error!(%error, "MakeService failed to become ready");
                    panic!("MakeService failed to become ready: {}", error);
                });
            let svc = self
                .make_service(target)
                .err_into()
                .await
                .unwrap_or_else(|error| {
                    tracing::error!(%error, target = %target_str, "making service failed");
                    panic!("making service for {} failed: {}", target_str, error);
                });
            let req_str = format!("{:?}", req);
            svc.oneshot(req).err_into().await.unwrap_or_else(|error| {
                tracing::error!(%error, request = %req_str, "request failed");
                panic!("request for {} failed: {}", req_str, error);
            })
        })
    }
}
