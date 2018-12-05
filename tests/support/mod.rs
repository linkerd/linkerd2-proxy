// The support mod is compiled for all the integration tests, which are each
// compiled as separate crates. Each only uses a subset of this module, which
// means some of it is unused.
//
// Note, lints like `unused_variable` should not be ignored.
#![allow(dead_code)]

pub extern crate bytes;
pub extern crate linkerd2_proxy_api;
extern crate linkerd2_proxy;
extern crate linkerd2_task;
extern crate futures;
extern crate h2;
pub extern crate http;
extern crate hyper;
pub extern crate net2;
extern crate prost;
extern crate tokio;
extern crate tokio_connect;
pub extern crate tokio_io;
extern crate tower_grpc;
extern crate tower_service;
extern crate log;
extern crate env_logger;

use std::fmt;
pub use std::collections::HashMap;
pub use std::net::SocketAddr;
pub use std::time::Duration;

pub use self::bytes::Bytes;
pub use self::linkerd2_proxy::*;
pub use self::linkerd2_task::LazyExecutor;
pub use self::futures::{future::Executor, *,};
pub use self::futures::sync::oneshot;
pub use self::http::{HeaderMap, Request, Response, StatusCode};
use self::tokio::{
    executor::current_thread,
    net::{TcpListener},
    runtime,
    reactor,
};
use self::tokio_connect::Connect;
use self::tower_grpc as grpc;
use self::tower_service::{Service};

/// Environment variable for overriding the test patience.
pub const ENV_TEST_PATIENCE_MS: &'static str = "RUST_TEST_PATIENCE_MS";
pub const DEFAULT_TEST_PATIENCE: Duration = Duration::from_millis(15);

/// By default, disable logging in modules that are expected to error in tests.
const DEFAULT_LOG: &'static str =
    "error,\
    linkerd2_proxy::proxy::canonicalize=off,\
    linkerd2_proxy::proxy::http::router=off,\
    linkerd2_proxy::proxy::tcp=off";

pub fn env_logger_init() {
    use std::env;

    let log = env::var("LINKERD2_PROXY_LOG").unwrap_or_else(|_| DEFAULT_LOG.to_owned());
    env::set_var("RUST_LOG", &log);
    env::set_var("LINKERD2_PROXY_LOG", &log);

    if let Err(e) = env_logger::try_init() {
        eprintln!("Failed to initialize logger: {}", e);
    }
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
        assert!($haystack.contains($needle), "haystack:\n{}\ndid not contain:\n{}", $haystack, $needle)
    }
}

#[macro_export]
macro_rules! assert_eventually_contains {
    ($scrape:expr, $contains:expr) => {
        assert_eventually!($scrape.contains($contains), "metrics scrape:\n{}\ndid not contain:\n{}", $scrape, $contains)
    }
}

pub mod client;
pub mod controller;
pub mod proxy;
pub mod server;
pub mod tap;
pub mod tcp;

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

pub type ShutdownRx = Box<Future<Item=(), Error=()> + Send>;

/// A channel used to signal when a Client's related connection is running or closed.
pub fn running() -> (oneshot::Sender<()>, Running) {
    let (tx, rx) = oneshot::channel();
    let rx = Box::new(rx.then(|_| Ok::<(), ()>(())));
    (tx, rx)
}

pub type Running = Box<Future<Item=(), Error=()> + Send>;

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
