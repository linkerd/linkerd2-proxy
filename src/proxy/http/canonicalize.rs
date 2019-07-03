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

use futures::{Async, Future, Poll, Stream};
use http;
use log::trace;
use std::time::Duration;
use tokio::{self, sync::mpsc};
use tokio_timer::{clock, Delay, Timeout};

use dns;
use svc;
use {Addr, NameAddr};

/// Duration to wait before polling DNS again after an error (or a NXDOMAIN
/// response with no TTL).
const DNS_ERROR_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Layer {
    resolver: dns::Resolver,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Stack<M> {
    resolver: dns::Resolver,
    inner: M,
    timeout: Duration,
}

pub struct MakeFuture<F> {
    inner: F,
    task: Option<(NameAddr, dns::Resolver, Duration)>,
}

pub struct Service<S> {
    canonicalized: Option<Addr>,
    inner: S,
    rx: mpsc::Receiver<NameAddr>,
}

struct Task {
    original: NameAddr,
    resolved: Cache,
    resolver: dns::Resolver,
    state: State,
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

enum State {
    Init,
    Pending(Timeout<dns::RefineFuture>),
    ValidUntil(Delay),
}

// === Layer ===

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
pub fn layer(resolver: dns::Resolver, timeout: Duration) -> Layer {
    Layer { resolver, timeout }
}

impl<M> svc::Layer<M> for Layer
where
    M: svc::Service<Addr> + Clone,
{
    type Service = Stack<M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            resolver: self.resolver.clone(),
            timeout: self.timeout,
        }
    }
}

// === impl Stack ===

impl<M> svc::Service<Addr> for Stack<M>
where
    M: svc::Service<Addr>,
{
    type Response = svc::Either<Service<M::Response>, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, addr: Addr) -> Self::Future {
        let task = match addr {
            Addr::Name(ref na) => Some((na.clone(), self.resolver.clone(), self.timeout)),
            Addr::Socket(_) => None,
        };

        let inner = self.inner.call(addr);
        MakeFuture { inner, task }
    }
}

// === impl MakeFuture ===

impl<F> Future for MakeFuture<F>
where
    F: Future,
{
    type Item = svc::Either<Service<F::Item>, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = if let Some((na, resolver, timeout)) = self.task.take() {
            let (tx, rx) = mpsc::channel(2);

            tokio::spawn(Task::new(na, resolver, timeout, tx));

            svc::Either::A(Service {
                canonicalized: None,
                inner,
                rx,
            })
        } else {
            svc::Either::B(inner)
        };

        Ok(svc.into())
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
            resolved: Cache::AwaitingInitial,
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
            // If the receiver has been dropped, stop watching for updates.
            let tx_ready = match self.tx.poll_ready() {
                Ok(r) => r,
                Err(_) => {
                    trace!("task complete; name={:?}", self.original);
                    return Ok(Async::Ready(()));
                }
            };

            self.state = match self.state {
                State::Init => {
                    trace!("task init; name={:?}", self.original);
                    let f = self.resolver.refine(self.original.name());
                    State::Pending(Timeout::new(f, self.timeout))
                }
                State::Pending(ref mut fut) => {
                    // Only poll the resolution for updates when the receiver is
                    // ready to receive an update.
                    if tx_ready.is_not_ready() {
                        trace!("task awaiting capacity; name={:?}", self.original);
                        return Ok(Async::NotReady);
                    }

                    match fut.poll() {
                        Ok(Async::NotReady) => {
                            return Ok(Async::NotReady);
                        }
                        Ok(Async::Ready(refine)) => {
                            trace!(
                                "task update; name={:?} refined={:?}",
                                self.original,
                                refine.name
                            );
                            // If the resolved name is a new name, bind a
                            // service with it and set a delay that will notify
                            // when the resolver should be consulted again.
                            let resolved = NameAddr::new(refine.name, self.original.port());
                            if self.resolved.get() != Some(&resolved) {
                                self.tx
                                    .try_send(resolved.clone())
                                    .expect("tx failed despite being ready");
                                self.resolved = Cache::Resolved(resolved);
                            }

                            State::ValidUntil(Delay::new(refine.valid_until))
                        }
                        Err(e) => {
                            trace!("task error; name={:?} err={:?}", self.original, e);

                            if self.resolved == Cache::AwaitingInitial {
                                // The service needs a value, so we need to
                                // publish the original name so it can proceed.
                                warn!(
                                    "failed to refine {}: {}; using original name",
                                    self.original.name(),
                                    e,
                                );
                                self.tx
                                    .try_send(self.original.clone())
                                    .expect("tx failed despite being ready");

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
                    trace!("task idle; name={:?}", self.original);

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

impl<S, B> svc::Service<http::Request<B>> for Service<S>
where
    S: svc::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        try_ready!(self.inner.poll_ready());

        while let Ok(Async::Ready(Some(addr))) = self.rx.poll() {
            debug!("refined: {}", addr);
            self.canonicalized = Some(addr.into());
        }
        if self.canonicalized.is_none() {
            return Ok(Async::NotReady);
        }

        Ok(Async::Ready(()))
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let addr = self
            .canonicalized
            .clone()
            .expect("called before canonicalized address");
        req.extensions_mut().insert(addr);
        self.inner.call(req)
    }
}

impl<S> Drop for Service<S> {
    fn drop(&mut self) {
        trace!("dropping service; name={:?}", self.canonicalized);
    }
}
