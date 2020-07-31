//! Shared infrastructure for integration tests

#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "256"]
#![type_length_limit = "16289823"]

mod test_env;

pub use self::test_env::TestEnv;
pub use bytes::{Buf, BufMut, Bytes};
pub use futures::{future, FutureExt, TryFuture, TryFutureExt};

pub use http::{HeaderMap, Request, Response, StatusCode};
pub use http_body::Body as HttpBody;
pub use linkerd2_app as app;
pub use linkerd2_app_core::drain;
pub use std::collections::HashMap;
use std::fmt;
pub use std::future::Future;
use std::io;
pub use std::net::SocketAddr;
use std::pin::Pin;
pub use std::sync::Arc;
use std::task::{Context, Poll};
pub use std::time::Duration;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
pub use tokio::stream::{Stream, StreamExt};
pub use tokio::sync::oneshot;
pub use tonic as grpc;
pub use tower::Service;
pub use tracing::*;
pub use tracing_subscriber::prelude::*;

/// Environment variable for overriding the test patience.
pub const ENV_TEST_PATIENCE_MS: &'static str = "RUST_TEST_PATIENCE_MS";
pub const DEFAULT_TEST_PATIENCE: Duration = Duration::from_millis(15);

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// By default, disable logging in modules that are expected to error in tests.
const DEFAULT_LOG: &'static str = "error,\
                                   linkerd2_proxy_http=off,\
                                   linkerd2_proxy_transport=off";

pub fn trace_subscriber() -> (Dispatch, app::core::trace::Handle) {
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
    let _ = app::core::trace::init_log_compat();
    app::core::trace::with_filter_and_format(&log_level, &log_format)
}

pub fn trace_init() -> tracing::dispatcher::DefaultGuard {
    let (d, _) = trace_subscriber();
    tracing::dispatcher::set_default(&d)
}

/// Retry an assertion up to a specified number of times, waiting
/// `RUST_TEST_PATIENCE_MS` between retries.
///
/// If the assertion is successful after a retry, execution will continue
/// normally. If all retries are exhausted and the assertion still fails,
/// `assert_eventually!` will panic as though a regular `assert!` had failed.
/// Note that other panics elsewhere in the code under test will not be
/// prevented.
///
/// This should be used sparingly, but is often useful in end-to-end testing
/// where a desired state may not be reached immediately. For example, when
/// some state updates asynchronously and there's no obvious way for the test
/// to wait for an update to occur before making assertions.
///
/// The `RUST_TEST_PATIENCE_MS` environment variable may be used to customize
/// the backoff duration between retries. This may be useful for purposes such
/// compensating for decreased performance on CI.
#[macro_export]
macro_rules! assert_eventually {
    ($cond:expr, retries: $retries:expr, $($arg:tt)+) => {
        {
            use std::{env, u64};
            use std::time::{Instant, Duration};
            use std::str::FromStr;
            use tracing_futures::Instrument as _;
            // TODO: don't do this *every* time eventually is called (lazy_static?)
            let patience = env::var($crate::ENV_TEST_PATIENCE_MS).ok()
                .map(|s| {
                    let millis = u64::from_str(&s)
                        .expect(
                            "Could not parse RUST_TEST_PATIENCE_MS environment \
                             variable."
                        );
                    Duration::from_millis(millis)
                })
                .unwrap_or($crate::DEFAULT_TEST_PATIENCE);
            async {
                let start_t = Instant::now();
                for i in 0i32..($retries as i32 + 1i32) {
                    tracing::info!(retries_remaining = $retries - i);
                    if $cond {
                        break;
                    } else if i == $retries {
                        panic!(
                            "assertion failed after {} (retried {} times): {}",
                            crate::HumanDuration(start_t.elapsed()),
                            i,
                            format_args!($($arg)+)
                        )
                    } else {
                        tracing::trace!("waiting...");
                        tokio::time::delay_for(patience).await;
                        std::thread::yield_now();
                        tracing::trace!("done");
                    }
                }
            }.instrument(tracing::trace_span!(
                "assert_eventually",
                patience  = %crate::HumanDuration(patience),
                max_retries = $retries
            ))
            .await
        }
    };
    ($cond:expr, $($arg:tt)+) => {
        assert_eventually!($cond, retries: 5, $($arg)+)
    };
    ($cond:expr, retries: $retries:expr) => {
        assert_eventually!($cond, retries: $retries, stringify!($cond))
    };
    ($cond:expr) => {
        assert_eventually!($cond, retries: 5, stringify!($cond))
    };
}

#[macro_export]
macro_rules! assert_contains {
    ($haystack:expr, $needle:expr) => {
        assert!(
            $haystack.contains($needle),
            "haystack:\n{}\ndid not contain:\n{}",
            $haystack,
            $needle
        )
    };
}

#[macro_export]
macro_rules! assert_eventually_contains {
    ($scrape:expr, $contains:expr) => {
        assert_eventually!(
            $scrape.contains($contains),
            "metrics scrape:\n{}\ndid not contain:\n{}",
            $scrape,
            $contains
        )
    };
}

pub mod client;
pub mod controller;
pub mod identity;
pub mod proxy;
pub mod server;
pub mod tap;
pub mod tcp;

trait Io: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> Io for T {}

struct RunningIo {
    pub io: Pin<Box<dyn Io + Send>>,
    pub abs_form: bool,
    pub _running: Option<oneshot::Sender<()>>,
}

impl AsyncRead for RunningIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.as_mut().io.as_mut().poll_read(cx, buf)
    }

    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [std::mem::MaybeUninit<u8>]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl AsyncWrite for RunningIo {
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().io.as_mut().poll_shutdown(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.as_mut().io.as_mut().poll_flush(cx)
    }

    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.as_mut().io.as_mut().poll_write(cx, buf)
    }
}

pub fn shutdown_signal() -> (Shutdown, ShutdownRx) {
    let (tx, rx) = oneshot::channel();
    (Shutdown { tx }, Box::pin(rx.map(|_| ())))
}

pub struct Shutdown {
    tx: oneshot::Sender<()>,
}

impl Shutdown {
    pub fn signal(self) {
        let _ = self.tx.send(());
    }
}

pub type ShutdownRx = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A channel used to signal when a Client's related connection is running or closed.
pub fn running() -> (oneshot::Sender<()>, Running) {
    let (tx, rx) = oneshot::channel();
    let rx = Box::pin(rx.map(|_| ()));
    (tx, rx)
}

pub type Running = Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>;

pub fn s(bytes: &[u8]) -> &str {
    ::std::str::from_utf8(bytes.as_ref()).unwrap()
}

/// The Rust test runner creates a thread per unit test, naming it after
/// the function name. If still in that thread, this can be useful to allow
/// associating test logs with a specific test, since tests *can* be run in
/// parallel.
pub fn thread_name() -> String {
    ::std::thread::current()
        .name()
        .unwrap_or("<no-name>")
        .to_owned()
}

#[tokio::test]
#[should_panic]
async fn test_assert_eventually() {
    assert_eventually!(false)
}

/// A duration which pretty-prints as fractional seconds.
#[derive(Copy, Clone, Debug)]
pub struct HumanDuration(pub Duration);

impl fmt::Display for HumanDuration {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "{}ms", subsec_ms)
        } else {
            write!(fmt, "{}s", secs as f64 + subsec_ms)
        }
    }
}

pub async fn cancelable<E: Send + 'static>(
    drain: drain::Watch,
    f: impl Future<Output = Result<(), E>> + Send + 'static,
) -> Result<(), E> {
    tokio::select! {
        res = f => res,
        _ = drain.signal() => {
            tracing::debug!("canceled!");
            Ok(())
        }
    }
}
