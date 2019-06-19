// The support mod is compiled for all the integration tests, which are each
// compiled as separate crates. Each only uses a subset of this module, which
// means some of it is unused.
//
// Note, lints like `unused_variable` should not be ignored.
#![allow(dead_code)]

pub extern crate bytes;
extern crate env_logger;
extern crate futures;
extern crate h2;
pub extern crate http;
extern crate http_body;
extern crate hyper;
extern crate linkerd2_proxy;
pub extern crate linkerd2_proxy_api;
extern crate linkerd2_task;
extern crate log;
pub extern crate net2;
extern crate prost;
pub extern crate rustls;
extern crate tokio;
extern crate tokio_connect;
extern crate tokio_current_thread;
pub extern crate tokio_io;
extern crate tokio_rustls;
pub extern crate tower_grpc;
extern crate tower_service;
extern crate webpki;
extern crate tokio_trace as trace;

pub use std::collections::HashMap;
use std::fmt;
pub use std::net::SocketAddr;
pub use std::time::Duration;

pub use self::bytes::Bytes;
pub use self::futures::sync::oneshot;
pub use self::futures::{future::Executor, *};
pub use self::http::{HeaderMap, Request, Response, StatusCode};
use self::http_body::Body as HttpBody;
pub use self::linkerd2_proxy::*;
pub use self::linkerd2_task::LazyExecutor;
use self::tokio::io::{AsyncRead, AsyncWrite};
use self::tokio::net::TcpStream;
use self::tokio::{net::TcpListener, reactor, runtime};
use self::tokio_connect::Connect;
use self::tokio_current_thread as current_thread;
use self::tokio_rustls::TlsStream;
pub use self::tower_grpc as grpc;
pub use self::tower_service::Service;
use rustls::Session;
use std::io;
use std::io::{Read, Write};

/// Environment variable for overriding the test patience.
pub const ENV_TEST_PATIENCE_MS: &'static str = "RUST_TEST_PATIENCE_MS";
pub const DEFAULT_TEST_PATIENCE: Duration = Duration::from_millis(15);

/// By default, disable logging in modules that are expected to error in tests.
const DEFAULT_LOG: &'static str = "error,\
                                   linkerd2_proxy::proxy::canonicalize=off,\
                                   linkerd2_proxy::proxy::http::router=off,\
                                   linkerd2_proxy::proxy::tcp=off";

pub fn env_logger_init() -> Result<(), String> {
    use std::env;
    use self::linkerd2_proxy::trace;

    let log = env::var("LINKERD2_PROXY_LOG")
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| DEFAULT_LOG.to_owned());
    env::set_var("RUST_LOG", &log);
    env::set_var("LINKERD2_PROXY_LOG", &log);

    trace::try_init_with_filter(&log).map_err(ToString::to_string)
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
            // TODO: don't do this *every* time eventually is called (lazy_static?)
            let patience = env::var($crate::support::ENV_TEST_PATIENCE_MS).ok()
                .map(|s| {
                    let millis = u64::from_str(&s)
                        .expect(
                            "Could not parse RUST_TEST_PATIENCE_MS environment \
                             variable."
                        );
                    Duration::from_millis(millis)
                })
                .unwrap_or($crate::support::DEFAULT_TEST_PATIENCE);
            let start_t = Instant::now();
            for i in 0..($retries + 1) {
                if $cond {
                    break;
                } else if i == $retries {
                    panic!(
                        "assertion failed after {} (retried {} times): {}",
                        ::support::HumanDuration(start_t.elapsed()),
                        i,
                        format_args!($($arg)+)
                    )
                } else {
                    ::std::thread::sleep(patience);
                }
            }
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

enum RunningIo<T> {
    Plain(TcpStream, Option<oneshot::Sender<()>>),
    Tls(TlsStream<TcpStream, T>, Option<oneshot::Sender<()>>),
}

impl<T: Session> Read for RunningIo<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            RunningIo::Plain(ref mut s, _) => s.read(buf),
            RunningIo::Tls(ref mut s, _) => s.read(buf),
        }
    }
}

impl<T: Session> Write for RunningIo<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            RunningIo::Plain(ref mut s, _) => s.write(buf),
            RunningIo::Tls(ref mut s, _) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            RunningIo::Plain(ref mut s, _) => s.flush(),
            RunningIo::Tls(ref mut s, _) => s.flush(),
        }
    }
}

impl<T: Session> AsyncRead for RunningIo<T> {}

impl<T: Session> AsyncWrite for RunningIo<T> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        match *self {
            RunningIo::Plain(ref mut s, _) => s.shutdown(),
            RunningIo::Tls(ref mut s, _) => s.shutdown(),
        }
    }
}

pub fn shutdown_signal() -> (Shutdown, ShutdownRx) {
    let (tx, rx) = oneshot::channel();
    (Shutdown { tx }, Box::new(rx.then(|_| Ok(()))))
}

pub struct Shutdown {
    tx: oneshot::Sender<()>,
}

impl Shutdown {
    pub fn signal(self) {
        // a drop is enough
    }
}

pub type ShutdownRx = Box<Future<Item = (), Error = ()> + Send>;

/// A channel used to signal when a Client's related connection is running or closed.
pub fn running() -> (oneshot::Sender<()>, Running) {
    let (tx, rx) = oneshot::channel();
    let rx = Box::new(rx.then(|_| Ok::<(), ()>(())));
    (tx, rx)
}

pub type Running = Box<Future<Item = (), Error = ()> + Send>;

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

#[test]
#[should_panic]
fn assert_eventually() {
    assert_eventually!(false)
}

/// A duration which pretty-prints as fractional seconds.
#[derive(Copy, Clone, Debug)]
pub struct HumanDuration(pub Duration);

impl fmt::Display for HumanDuration {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let secs = self.0.as_secs();
        let subsec_ms = self.0.subsec_nanos() as f64 / 1_000_000f64;
        if secs == 0 {
            write!(fmt, "{}ms", subsec_ms)
        } else {
            write!(fmt, "{}s", secs as f64 + subsec_ms)
        }
    }
}

pub trait FutureWaitExt: Future {
    fn wait_timeout(self, dur: Duration) -> Result<Self::Item, Waited<Self::Error>>
    where
        Self: Sized,
    {
        use std::sync::Arc;
        use std::thread;
        use std::time::Instant;

        struct ThreadNotify(thread::Thread);

        impl futures::executor::Notify for ThreadNotify {
            fn notify(&self, _id: usize) {
                self.0.unpark();
            }
        }

        let deadline = Instant::now() + dur;
        let mut task = futures::executor::spawn(self);
        let notify = Arc::new(ThreadNotify(thread::current()));

        loop {
            match task.poll_future_notify(&notify, 0)? {
                Async::Ready(val) => return Ok(val),
                Async::NotReady => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(Waited::TimedOut);
                    }

                    thread::park_timeout(deadline - now);
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum Waited<E> {
    Error(E),
    TimedOut,
}

impl<E> From<E> for Waited<E> {
    fn from(err: E) -> Waited<E> {
        Waited::Error(err)
    }
}

impl<E: fmt::Display> fmt::Display for Waited<E> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Waited::Error(ref e) => fmt::Display::fmt(e, fmt),
            Waited::TimedOut => fmt.write_str("wait timed out"),
        }
    }
}

impl<T: Future> FutureWaitExt for T {}

pub trait ResultWaitedExt {
    fn expect_timedout(self, msg: &str);
}

impl<T: fmt::Debug, E: fmt::Debug> ResultWaitedExt for Result<T, Waited<E>> {
    fn expect_timedout(self, msg: &str) {
        match self {
            Ok(val) => panic!("{}; expected TimedOut, was Ok({:?})", msg, val),
            Err(Waited::Error(err)) => {
                panic!("{}; expected TimedOut, was Error({:?})", msg, err);
            }
            Err(Waited::TimedOut) => (),
        }
    }
}
