use futures::{future::MapErr, TryFutureExt};
use linkerd_error::Error;
use std::{
    future::Future,
    task::{Context, Poll},
};

/// A middleware type that cannot exert backpressure.
///
/// Typically used to modify requests or responses.
pub trait Proxy<Req, S: tower::Service<Self::Request>> {
    /// The type of request sent to the inner `S`-typed service.
    type Request;

    /// The type of response returned to callers.
    type Response;

    /// The error type returned to callers.
    type Error: Into<Error>;

    /// The Future type returned to callers.
    type Future: Future<Output = Result<Self::Response, Self::Error>>;

    /// Usually invokes `S::call`, potentially modifying requests or responses.
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future;

    /// Composes this `Proxy` with a [`Service`], returning a new [`Service`]
    /// that calls the provided [`Service`] through this `Proxy`.
    ///
    /// [`Service`]: tower::Service
    fn into_service(self, svc: S) -> ProxyService<Self, S>
    where
        Self: Sized,
    {
        ProxyService { proxy: self, svc }
    }

    /// Like [`ServiceExt::oneshot`](tower::util::ServiceExt::oneshot), but with
    /// a `Proxy` too.
    fn proxy_oneshot(self, svc: S, req: Req) -> Oneshot<Self, S, Req>
    where
        Self: Sized,
    {
        Oneshot {
            req: Some(req),
            state: State::PollReady { proxy: self, svc },
        }
    }
}

/// Composes a [`Proxy`] with a [`Service`] to create a new [`Service`].
///
/// [`Service`]: tower::Service
#[derive(Clone, Debug)]
pub struct ProxyService<P, S> {
    proxy: P,
    svc: S,
}

#[pin_project::pin_project]
pub struct Oneshot<P, S, R>
where
    P: Proxy<R, S>,
    S: tower::Service<P::Request>,
{
    req: Option<R>,
    #[pin]
    state: State<P, S, P::Future>,
}

// === impl Proxy ===

/// The identity Proxy.
impl<Req, S> Proxy<Req, S> for ()
where
    S: tower::Service<Req>,
    S::Error: Into<Error>,
{
    type Request = Req;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn proxy(&self, inner: &mut S, req: Req) -> Self::Future {
        inner.call(req)
    }
}

// === impl ProxyService ===

impl<P, S, R> tower::Service<R> for ProxyService<P, S>
where
    P: Proxy<R, S>,
    S: tower::Service<P::Request>,
    Error: From<S::Error>,
{
    type Response = P::Response;
    type Error = Error;
    type Future = MapErr<P::Future, fn(P::Error) -> Error>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.svc.poll_ready(cx).map_err(Into::into)
    }

    #[inline]
    fn call(&mut self, req: R) -> Self::Future {
        self.proxy.proxy(&mut self.svc, req).map_err(Into::into)
    }
}

// === impl Oneshot ===

#[pin_project::pin_project(project = StateProj)]
enum State<P, S, F> {
    PollReady { proxy: P, svc: S },
    Future(#[pin] F),
}

impl<P, S, R> Future for Oneshot<P, S, R>
where
    P: Proxy<R, S>,
    S: tower::Service<P::Request>,
    P::Error: From<S::Error>,
{
    type Output = Result<P::Response, P::Error>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut this = self.project();
        loop {
            match this.state.as_mut().project() {
                StateProj::PollReady { proxy, svc } => {
                    futures::ready!(svc.poll_ready(cx))?;
                    let f = proxy.proxy(svc, this.req.take().expect("already called"));
                    this.state.as_mut().set(State::Future(f));
                }
                StateProj::Future(f) => return f.poll(cx),
            }
        }
    }
}
