//! A stack module that lazily, dynamically resolves an `Addr` target, via DNS,
//! to determine it's canonical fully qualified domain name.
//!
//! For example, an application may set an authority value like `web:8080` with a
//! resolv.conf(5) search path of `example.com example.net`. In such a case,
//! this module may build its inner stack with either `web.example.com.:8080`,
//! `web.example.net.:8080`, or `web:8080`, depending on the state of DNS.

use futures::{ready, TryFuture};
use linkerd2_addr::{Addr, NameAddr};
use linkerd2_dns::Name;
use linkerd2_error::Error;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{self, Timeout};
use tracing::{debug, info};

pub trait Target {
    fn addr(&self) -> &Addr;
    fn addr_mut(&mut self) -> &mut Addr;
}

impl<T: AsRef<Addr> + AsMut<Addr>> Target for T {
    fn addr(&self) -> &Addr {
        self.as_ref()
    }

    fn addr_mut(&mut self) -> &mut Addr {
        self.as_mut()
    }
}

// FIXME the resolver should be abstracted to a trait so that this can be tested
// without a real DNS service.
#[derive(Debug, Clone)]
pub struct Layer<R> {
    resolver: R,
    timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Canonicalize<R, M> {
    inner: M,
    resolver: R,
    timeout: Duration,
}

#[pin_project]
pub struct MakeFuture<T, R, M: tower::Service<T>> {
    #[pin]
    state: State<T, R, M>,
}

#[pin_project(project = StateProj)]
enum State<T, R, M: tower::Service<T>> {
    Refine {
        #[pin]
        future: Timeout<R>,
        make: Option<M>,
        original: Option<T>,
    },
    NotReady(M, Option<T>),
    Make(#[pin] M::Future),
}

// === Layer ===

impl<R> Layer<R> {
    pub fn new(resolver: R, timeout: Duration) -> Self {
        Layer { resolver, timeout }
    }
}

impl<R: Clone, M> tower::layer::Layer<M> for Layer<R> {
    type Service = Canonicalize<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            timeout: self.timeout,
            resolver: self.resolver.clone(),
        }
    }
}

// === impl Canonicalize ===

impl<T, R, M> tower::Service<T> for Canonicalize<R, M>
where
    T: Target + Clone,
    R: tower::Service<Name, Response = Name> + Clone,
    R::Error: Into<Error>,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Response = M::Response;
    type Error = Error;
    type Future = MakeFuture<T, R::Future, M>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.resolver.poll_ready(cx)).map_err(Into::into)?;
        ready!(self.inner.poll_ready(cx)).map_err(Into::into)?;
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        match target.addr().name_addr() {
            None => {
                self.resolver = self.resolver.clone();
                MakeFuture {
                    state: State::Make(self.inner.call(target)),
                }
            }
            Some(na) => {
                let refine = self.resolver.call(na.name().clone());

                let inner = self.inner.clone();
                let make = std::mem::replace(&mut self.inner, inner);

                MakeFuture {
                    state: State::Refine {
                        make: Some(make),
                        original: Some(target.clone()),
                        future: time::timeout(self.timeout, refine),
                    },
                }
            }
        }
    }
}

// === impl MakeFuture ===

impl<T, R, M, E> Future for MakeFuture<T, R, M>
where
    T: Target,
    R: Future<Output = Result<Name, E>>,
    E: Into<Error>,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Output = Result<M::Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.project().state;
        loop {
            match state.as_mut().project() {
                StateProj::Refine {
                    future,
                    make,
                    original,
                } => {
                    let target = match ready!(future.poll(cx)) {
                        Ok(Ok(refined)) => {
                            let mut target = original.take().expect("illegal state");
                            let name = NameAddr::new(refined, target.addr().port());
                            *target.addr_mut() = name.into();
                            target
                        }
                        Ok(Err(error)) => {
                            let error = error.into();
                            debug!(%error, "DNS refinement failed");
                            original.take().expect("illegal state")
                        }
                        Err(_) => {
                            info!("DNS refinement timed out");
                            original.take().expect("illegal state")
                        }
                    };

                    let make = make.take().expect("illegal state");
                    state.set(State::NotReady(make, Some(target)));
                }
                StateProj::NotReady(svc, target) => {
                    ready!(svc.poll_ready(cx)).map_err(Into::into)?;
                    let target = target.take().expect("illegal state");
                    let fut = svc.call(target);
                    state.set(State::Make(fut));
                }
                StateProj::Make(fut) => return fut.try_poll(cx).map_err(Into::into),
            };
        }
    }
}
