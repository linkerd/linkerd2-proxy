use std::{
    fmt,
    io,
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::{Async, Future, Poll};
use h2;
use http;
use tokio::timer::Delay;

use tower_h2::{self, BoxBody, RecvBody};
use tower_add_origin::AddOrigin;
use tower_service::Service;
use tower_reconnect::{
    Reconnect,
    Error as ReconnectError,
    ResponseFuture as ReconnectFuture,
};
use conditional::Conditional;
use dns;
use timeout::{Timeout, Error as TimeoutError};
use transport::{tls, HostAndPort, LookupAddressAndConnect};
use watch_service::Rebind;

/// Type of the client service stack used to make destination requests.
pub(super) struct ClientService(AddOrigin<Backoff<LogErrors<Reconnect<
        tower_h2::client::Connect<
            Timeout<LookupAddressAndConnect>,
            ::logging::ContextualExecutor<
                ::logging::Client<
                    &'static str,
                    HostAndPort
                >
            >,
            BoxBody,
        >
    >>>>);

/// The state needed to bind a new controller client stack.
pub(super) struct BindClient {
    backoff_delay: Duration,
    identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
    host_and_port: HostAndPort,
    dns_resolver: dns::Resolver,
    log_ctx: ::logging::Client<&'static str, HostAndPort>,
}

/// Wait a duration if inner `poll_ready` returns an error.
//TODO: move to tower-backoff
struct Backoff<S> {
    inner: S,
    timer: Delay,
    waiting: bool,
    wait_dur: Duration,
}

/// Log errors talking to the controller in human format.
struct LogErrors<S> {
    inner: S,
}

// We want some friendly logs, but the stack of services don't have fmt::Display
// errors, so we have to build that ourselves. For now, this hard codes the
// expected error stack, and so any new middleware added will need to adjust this.
//
// The dead_code allowance is because rustc is being stupid and doesn't see it
// is used down below.
#[allow(dead_code)]
type LogError = ReconnectError<
    tower_h2::client::Error,
    tower_h2::client::ConnectError<
        TimeoutError<
            io::Error
        >
    >
>;

// ===== impl ClientService =====

impl Service for ClientService {
    type Request = http::Request<BoxBody>;
    type Response = http::Response<RecvBody>;
    type Error = LogError;
    type Future = ReconnectFuture<
        tower_h2::client::Connect<
            Timeout<LookupAddressAndConnect>,
            ::logging::ContextualExecutor<
                ::logging::Client<
                    &'static str,
                    HostAndPort,
                >
            >, BoxBody>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, request: Self::Request) -> Self::Future {
        self.0.call(request)
    }
}

// ===== impl BindClient =====

impl BindClient {
    pub(super) fn new(
        identity: Conditional<tls::Identity, tls::ReasonForNoTls>,
        dns_resolver: &dns::Resolver,
        host_and_port: HostAndPort,
        backoff_delay: Duration,
    ) -> Self {
        let log_ctx = ::logging::admin().client("control", host_and_port.clone());
        Self {
            backoff_delay,
            identity,
            dns_resolver: dns_resolver.clone(),
            host_and_port,
            log_ctx,
        }
    }
}

impl Rebind<tls::ConditionalClientConfig> for BindClient {
    type Service = ClientService;
    fn rebind(
        &mut self,
        client_cfg: &tls::ConditionalClientConfig,
    ) -> Self::Service {
        let conn_cfg = match (&self.identity, client_cfg) {
            (Conditional::Some(ref id), Conditional::Some(ref cfg)) =>
                Conditional::Some(tls::ConnectionConfig {
                    server_identity: id.clone(),
                    config: cfg.clone(),
                }),
            (Conditional::None(ref reason), _) |
            (_, Conditional::None(ref reason)) =>
                Conditional::None(reason.clone()),
        };
        let scheme = http::uri::Scheme::from_shared(Bytes::from_static(b"http")).unwrap();
        let authority = http::uri::Authority::from(&self.host_and_port);
        let connect = Timeout::new(
            LookupAddressAndConnect::new(self.host_and_port.clone(),
                                         self.dns_resolver.clone(),
                                         conn_cfg),
            Duration::from_secs(3),
        );
        let h2_client = tower_h2::client::Connect::new(
            connect,
            h2::client::Builder::default(),
            self.log_ctx.clone().executor()
        );

        let reconnect = Reconnect::new(h2_client);
        let log_errors = LogErrors::new(reconnect);
        let backoff = Backoff::new(log_errors, self.backoff_delay);
        ClientService(AddOrigin::new(backoff, scheme, authority))
    }

}

// ===== impl Backoff =====

impl<S> Backoff<S>
where
    S: Service,
{
    fn new(inner: S, wait_dur: Duration) -> Self {
        Backoff {
            inner,
            timer: Delay::new(Instant::now() + wait_dur),
            waiting: false,
            wait_dur,
        }
    }

    fn poll_timer(&mut self) -> Poll<(), S::Error> {
        debug_assert!(self.waiting, "poll_timer expects to be waiting");

        match self.timer.poll().expect("timer shouldn't error") {
            Async::Ready(()) => {
                self.waiting = false;
                Ok(Async::Ready(()))
            }
            Async::NotReady => {
                Ok(Async::NotReady)
            }
        }
    }
}

impl<S> Service for Backoff<S>
where
    S: Service,
    S::Error: fmt::Debug,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        if self.waiting {
            try_ready!(self.poll_timer());
        }

        match self.inner.poll_ready() {
            Err(_err) => {
                trace!("backoff: controller error, waiting {:?}", self.wait_dur);
                self.waiting = true;
                self.timer.reset(Instant::now() + self.wait_dur);
                self.poll_timer()
            }
            ok => ok,
        }
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}


// ===== impl LogErrors =====

impl<S> LogErrors<S>
where
    S: Service<Error=LogError>,
{
    fn new(service: S) -> Self {
        LogErrors {
            inner: service,
        }
    }
}

impl<S> Service for LogErrors<S>
where
    S: Service<Error=LogError>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready().map_err(|e| {
            error!("controller error: {}", HumanError(&e));
            e
        })
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.inner.call(req)
    }
}

struct HumanError<'a>(&'a LogError);

impl<'a> fmt::Display for HumanError<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.0 {
            ReconnectError::Inner(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::Connect(ref e) => {
                fmt::Display::fmt(e, f)
            },
            ReconnectError::NotReady => {
                // this error should only happen if we `call` the service
                // when it isn't ready, which is really more of a bug on
                // our side...
                f.pad("bug: called service when not ready")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use tokio::runtime::current_thread::Runtime;

    struct MockService {
        polls: usize,
        succeed_after: usize,
    }

    impl Service for MockService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = future::FutureResult<(), ()>;

        fn poll_ready(&mut self) -> Poll<(), ()> {
            self.polls += 1;
            if self.polls > self.succeed_after {
                Ok(().into())
            } else {
                Err(())
            }
        }

        fn call(&mut self, _req: ()) -> Self::Future {
            if self.polls > self.succeed_after {
                future::ok(())
            } else {
                future::err(())
            }
        }
    }

    fn succeed_after(cnt: usize) -> MockService {
        MockService {
            polls: 0,
            succeed_after: cnt,
        }
    }


    #[test]
    fn backoff() {
        let mock = succeed_after(2);
        let mut backoff = Backoff::new(mock, Duration::from_millis(5));
        let mut rt = Runtime::new().unwrap();

        // The simple existance of this test checks that `Backoff` doesn't
        // hang after seeing an error in `inner.poll_ready()`, but registers
        // to poll again.
        rt.block_on(future::poll_fn(|| backoff.poll_ready())).unwrap();
    }
}

