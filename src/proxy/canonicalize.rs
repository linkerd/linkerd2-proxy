//! A stack module that lazily, dynamically resolves an `Addr` target, via DNS,
//! to determine it's canonical fully qualified domain name.
//!
//! For example, an application may set an authority value like `web:8080` with a
//! resolv.conf(5) search path of `example.com example.net`. In such a case,
//! this module may build its inner stack with either `web.example.com.:8080`,
//! `web.example.net.:8080`, or `web:8080`, depending on the state of DNS.
//!
//! DNS TTLs are honored and, if the resolution changes, the inner stack is
//! rebuilt with the updated value.

use futures::{future, sync::mpsc, Async, Future, Poll, Stream};
use std::time::Duration;
use tokio::executor::{DefaultExecutor, Executor};
use tokio_timer::{clock, Delay, Timeout};

use dns;
use svc;
use {Addr, NameAddr};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Duration to wait before polling DNS again after an error (or a NXDOMAIN
/// response with no TTL).
const DNS_ERROR_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Layer<R> {
    resolver: R,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M: svc::Stack<Addr>, R> {
    resolver: R,
    inner: M,
    timeout: Duration,
}

pub struct Service<M: svc::Stack<Addr>> {
    rx: mpsc::Receiver<NameAddr>,
    stack: M,
    service: Option<M::Value>,
}

struct Task<R: dns::Refine> {
    original: NameAddr,
    resolved: Cache,
    resolver: R,
    state: State<R::Future>,
    timeout: Duration,
    tx: mpsc::Sender<NameAddr>,
}

/// Tracks the state of the last resolution.
#[derive(Debug, Clone, Eq, PartialEq)]
enum Cache {
    /// The service has not yet been notified of a value.
    AwaitingInitial,

    /// The service has been notified with the original value (i.e. due to an
    /// error), and we do not yet have a resolved name.
    Unresolved,

    /// The service was last-notified with this name.
    Resolved(NameAddr),
}

enum State<F> {
    Init,
    Pending(Timeout<F>),
    ValidUntil(Delay),
}

// === Layer ===

pub fn layer<R>(resolver: R, timeout: Duration) -> Layer<R>
where
    R: dns::Refine + Clone + Send + 'static,
    R::Future: Send + 'static,
{
    Layer { resolver, timeout }
}

impl<M, R> svc::Layer<Addr, Addr, M> for Layer<R>
where
    M: svc::Stack<Addr> + Clone,
    R: dns::Refine + Clone + Send + 'static,
    R::Future: Send + 'static,
{
    type Value = <Stack<M, R> as svc::Stack<Addr>>::Value;
    type Error = <Stack<M, R> as svc::Stack<Addr>>::Error;
    type Stack = Stack<M, R>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            resolver: self.resolver.clone(),
            timeout: self.timeout,
        }
    }
}

// === impl Stack ===

impl<M, R> svc::Stack<Addr> for Stack<M, R>
where
    M: svc::Stack<Addr> + Clone,
    R: dns::Refine + Clone + Send + 'static,
    R::Future: Send + 'static,
{
    type Value = svc::Either<Service<M>, M::Value>;
    type Error = M::Error;

    fn make(&self, addr: &Addr) -> Result<Self::Value, Self::Error> {
        match addr {
            Addr::Name(na) => {
                let (tx, rx) = mpsc::channel(2);

                DefaultExecutor::current()
                    .spawn(Box::new(Task::new(
                        na.clone(),
                        self.resolver.clone(),
                        self.timeout,
                        tx,
                    )))
                    .expect("must be able to spawn");

                let svc = Service {
                    rx,
                    stack: self.inner.clone(),
                    service: None,
                };
                Ok(svc::Either::A(svc))
            }
            Addr::Socket(_) => self.inner.make(&addr).map(svc::Either::B),
        }
    }
}

// === impl Task ===

impl<R> Task<R>
where
    R: dns::Refine,
{
    fn new(original: NameAddr, resolver: R, timeout: Duration, tx: mpsc::Sender<NameAddr>) -> Self {
        Self {
            original,
            resolved: Cache::AwaitingInitial,
            resolver,
            state: State::Init,
            timeout,
            tx,
        }
    }
}

impl<R> Future for Task<R>
where
    R: dns::Refine,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            self.state = match self.state {
                State::Init => {
                    let f = self.resolver.refine(self.original.name());
                    State::Pending(Timeout::new(f, self.timeout))
                }
                State::Pending(ref mut fut) => {
                    match fut.poll() {
                        Ok(Async::NotReady) => {
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(refine)) => {
                            // If the resolved name is a new name, bind a
                            // service with it and set a delay that will notify
                            // when the resolver should be consulted again.
                            let resolved = NameAddr::new(refine.name, self.original.port());
                            if self.resolved.get() != Some(&resolved) {
                                let err = self.tx.try_send(resolved.clone()).err();
                                if err.map(|e| e.is_disconnected()).unwrap_or(false) {
                                    return Ok(().into());
                                }

                                self.resolved = Cache::Resolved(resolved);
                            }

                            State::ValidUntil(Delay::new(refine.valid_until))
                        }
                        Err(e) => {
                            if self.resolved == Cache::AwaitingInitial {
                                // The service needs a value, so we need to
                                // publish the original name so it can proceed.
                                warn!(
                                    "failed to refine {}: {}; using original name",
                                    self.original.name(),
                                    e,
                                );
                                let err = self.tx.try_send(self.original.clone()).err();
                                if err.map(|e| e.is_disconnected()).unwrap_or(false) {
                                    return Ok(().into());
                                }

                                // There's now no need to re-publish the
                                // original name on subsequent failures.
                                self.resolved = Cache::Unresolved;
                            } else {
                                debug!(
                                    "failed to refresh {}: {}; cache={:?}",
                                    self.original.name(),
                                    e,
                                    self.resolved,
                                );
                            }

                            let valid_until = e
                                .into_inner()
                                .and_then(|e| match e.kind() {
                                    dns::ResolveErrorKind::NoRecordsFound {
                                        valid_until, ..
                                    } => *valid_until,
                                    _ => None,
                                })
                                .unwrap_or_else(|| clock::now() + DNS_ERROR_TTL);

                            State::ValidUntil(Delay::new(valid_until))
                        }
                    }
                }

                State::ValidUntil(ref mut f) => {
                    match f.poll().expect("timer must not fail") {
                        Async::NotReady => return Ok(Async::NotReady),
                        Async::Ready(()) => {
                            // The last resolution's TTL expired, so issue a new DNS query.
                            State::Init
                        }
                    }
                }
            };
        }
    }
}

impl Cache {
    fn get(&self) -> Option<&NameAddr> {
        match self {
            Cache::Resolved(ref r) => Some(&r),
            _ => None,
        }
    }
}

// === impl Service ===

impl<M, Req, Svc> svc::Service<Req> for Service<M>
where
    M: svc::Stack<Addr, Value = Svc>,
    M::Error: Into<Error>,
    Svc: svc::Service<Req>,
    Svc::Error: Into<Error>,
{
    type Response = <M::Value as svc::Service<Req>>::Response;
    type Error = Error;
    type Future = future::MapErr<
        <M::Value as svc::Service<Req>>::Future,
        fn(<M::Value as svc::Service<Req>>::Error) -> Self::Error,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        while let Ok(Async::Ready(Some(addr))) = self.rx.poll() {
            debug!("refined: {}", addr);
            let svc = self.stack.make(&addr.into()).map_err(Into::into)?;
            self.service = Some(svc);
        }

        match self.service.as_mut() {
            Some(ref mut svc) => svc.poll_ready().map_err(Into::into),
            None => {
                trace!("resolution has not completed");
                Ok(Async::NotReady)
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.service
            .as_mut()
            .expect("poll_ready must be called first")
            .call(req)
            .map_err(Into::into)
    }
}
