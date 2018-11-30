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
use std::{error, fmt};
use tokio::executor::{DefaultExecutor, Executor};
use tokio_timer::{clock, Delay, Timeout};

use dns;
use svc;
use {Addr, NameAddr};

/// The amount of time to wait for a DNS query to succeed before falling back to
/// an uncanonicalized address.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);

/// Duration to wait before polling DNS again after an error (or a NXDOMAIN
/// response with no TTL).
const DNS_ERROR_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Layer {
    resolver: dns::Resolver,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M: svc::Stack<Addr>> {
    resolver: dns::Resolver,
    inner: M,
    timeout: Duration,
}

pub struct Service<M: svc::Stack<Addr>> {
    rx: mpsc::Receiver<NameAddr>,
    stack: M,
    service: Option<M::Value>,
}

struct Task {
    original: NameAddr,
    resolved: Option<NameAddr>,
    resolver: dns::Resolver,
    state: State,
    timeout: Duration,
    tx: mpsc::Sender<NameAddr>,
}

enum State {
    Init,
    Pending(Timeout<dns::RefineFuture>),
    ValidUntil(Delay),
}

#[derive(Debug)]
pub enum Error<M, S> {
    Stack(M),
    Service(S),
}

// === Layer ===

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
pub fn layer(resolver: dns::Resolver) -> Layer {
    Layer {
        resolver,
        timeout: DEFAULT_TIMEOUT,
    }
}

impl<M> svc::Layer<Addr, Addr, M> for Layer
where
    M: svc::Stack<Addr> + Clone,
{
    type Value = <Stack<M> as svc::Stack<Addr>>::Value;
    type Error = <Stack<M> as svc::Stack<Addr>>::Error;
    type Stack = Stack<M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            resolver: self.resolver.clone(),
            timeout: self.timeout,
        }
    }
}

// === impl Stack ===

impl<M> svc::Stack<Addr> for Stack<M>
where
    M: svc::Stack<Addr> + Clone,
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

impl Task {
    fn new(
        original: NameAddr,
        resolver: dns::Resolver,
        timeout: Duration,
        tx: mpsc::Sender<NameAddr>,
    ) -> Self {
        Self {
            original,
            resolved: None,
            resolver,
            state: State::Init,
            timeout,
            tx,
        }
    }
}

impl Future for Task {
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
                            if self.resolved.as_ref() != Some(&resolved) {
                                let err = self.tx.try_send(resolved.clone()).err();
                                if err.map(|e| e.is_disconnected()).unwrap_or(false) {
                                    return Ok(().into());
                                }

                                self.resolved = Some(resolved);
                            }

                            State::ValidUntil(Delay::new(refine.valid_until))
                        }
                        Err(e) => {
                            error!("failed to refine {}: {}", self.original.name(), e);
                            if self.resolved.is_none() {
                                let err = self.tx.try_send(self.original.clone()).err();
                                if err.map(|e| e.is_disconnected()).unwrap_or(false) {
                                    return Ok(().into());
                                }

                                // Pretend the original name was resolved so
                                // that we don't re-publish on subsequent errors.
                                self.resolved = Some(self.original.clone());
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

// === impl Service ===

impl<M, Req> svc::Service<Req> for Service<M>
where
    M: svc::Stack<Addr>,
    M::Value: svc::Service<Req>,
{
    type Response = <M::Value as svc::Service<Req>>::Response;
    type Error = Error<M::Error, <M::Value as svc::Service<Req>>::Error>;
    type Future = future::MapErr<
        <M::Value as svc::Service<Req>>::Future,
        fn(<M::Value as svc::Service<Req>>::Error) -> Self::Error,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        while let Ok(Async::Ready(Some(addr))) = self.rx.poll() {
            debug!("refined: {}", addr);
            let svc = self.stack.make(&addr.into()).map_err(Error::Stack)?;
            self.service = Some(svc);
        }

        match self.service.as_mut() {
            Some(ref mut svc) => svc.poll_ready().map_err(Error::Service),
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
            .map_err(Error::Service)
    }
}

// === impl Error ===

impl<M: fmt::Display, S: fmt::Display> fmt::Display for Error<M, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Stack(e) => e.fmt(f),
            Error::Service(e) => e.fmt(f),
        }
    }
}

impl<M: error::Error, S: error::Error> error::Error for Error<M, S> {
    fn cause(&self) -> Option<&error::Error> {
        match self {
            Error::Stack(e) => e.cause(),
            Error::Service(e) => e.cause(),
        }
    }
}
