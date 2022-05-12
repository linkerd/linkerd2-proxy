//! Shared infrastructure for integration tests

#![warn(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod test_env;

#[cfg(test)]
mod tests;

pub use self::test_env::TestEnv;
pub use bytes::{Buf, BufMut, Bytes};
pub use futures::{future, FutureExt, TryFuture, TryFutureExt};

pub use futures::stream::{Stream, StreamExt};
pub use http::{HeaderMap, Request, Response, StatusCode};
pub use http_body::Body as HttpBody;
pub use linkerd_app::{
    self as app,
    core::{drain, Addr},
};
pub use linkerd_app_test::*;
pub use linkerd_tracing::test::*;
use socket2::Socket;
pub use std::collections::HashMap;
use std::fmt;
pub use std::future::Future;
use std::io;
pub use std::net::SocketAddr;
use std::pin::Pin;
pub use std::sync::Arc;
use std::task::{Context, Poll};
pub use std::time::Duration;
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpListener;
pub use tokio::sync::oneshot;
pub use tonic as grpc;
pub use tower::Service;
pub use tracing::*;

/// Environment variable for overriding the test patience.
pub const ENV_TEST_PATIENCE_MS: &str = "RUST_TEST_PATIENCE_MS";
pub const DEFAULT_TEST_PATIENCE: Duration = Duration::from_millis(15);

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

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
            use std::str::FromStr;
            use tokio::time::{Instant, Duration};
            use tracing::Instrument as _;
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
                            crate::HumanDuration(Instant::now().saturating_duration_since(start_t)),
                            i,
                            format_args!($($arg)+)
                        )
                    } else {
                        tracing::trace!("waiting...");
                        tokio::time::sleep(patience).await;
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
    ($scrape:expr, $contains:expr) => {{
        let mut res: Result<(), crate::metrics::MatchErr> = Ok(());
        let res_ref = &mut res;
        assert_eventually!(
            {
                *res_ref = $contains.is_in($scrape);
                res_ref.is_ok()
            },
            "{}",
            std::mem::replace(res_ref, Ok(())).unwrap_err(),
        )
    }};
}

pub mod client;
pub mod controller;
pub mod identity;
pub mod metrics;
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
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.as_mut().io.as_mut().poll_read(cx, buf)
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
    ::std::str::from_utf8(bytes).unwrap()
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
        _ = drain.signaled() => {
            tracing::debug!("canceled!");
            Ok(())
        }
    }
}

/// Binds a socket to an ephemeral port without starting listening on it.
///
/// Some tests require us to introduce a delay between binding the server
/// socket and listening on it. However, `tokio::net`'s
/// `TcpListener::bind` function both binds the socket _and_ starts
/// listening on it, so we can't just use that. Instead, we must manually
/// construct the socket using `socket2`'s lower-level interface to libc
///  socket APIs, and _then_ convert it into a Tokio `TcpListener`.
pub(crate) fn bind_ephemeral() -> (Socket, SocketAddr) {
    use socket2::{Domain, Protocol, Type};

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("Socket::new");
    sock.bind(&addr.into()).expect("Socket::bind");
    let addr = sock
        .local_addr()
        .expect("Socket::local_addr")
        .as_socket()
        .expect("must be AF_INET");
    (sock, addr)
}

/// Start listening on a socket previously bound using `socket2`, returning a
/// Tokio `TcpListener`.
pub(crate) fn listen(sock: Socket) -> TcpListener {
    sock.listen(1024)
        .expect("socket should be able to start listening");
    sock.set_nonblocking(true)
        .expect("socket should be able to set nonblocking");
    TcpListener::from_std(sock.into()).expect("socket should seem okay to tokio")
}
