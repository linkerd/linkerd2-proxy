use crate::policy::{AllowPolicy, Permit};
use futures::future;
use linkerd_app_core::{
    svc, tls,
    transport::{ClientAddr, Remote},
};
use std::task;

#[derive(Clone, Debug)]
pub struct NewAuthorizeTcp<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub enum AuthorizeTcp<S> {
    Authorized(S),
    Unauthorized,
}

// === impl NewAuthorizeTcp ===

impl<N> NewAuthorizeTcp<N> {
    // FIXME metrics
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(|inner| Self { inner })
    }
}

impl<T, N> svc::NewService<T> for NewAuthorizeTcp<N>
where
    T: svc::Param<AllowPolicy>
        + svc::Param<Remote<ClientAddr>>
        + svc::Param<tls::ConditionalServerTls>,
    N: svc::NewService<(Permit, T)>,
{
    type Service = AuthorizeTcp<N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let client_addr = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        match policy.check_authorized(client_addr, &tls) {
            Ok(permit) => AuthorizeTcp::Authorized(self.inner.new_service((permit, target))),
            Err(_denied) => AuthorizeTcp::Unauthorized,
        }
    }
}

// === impl AuthorizeTcp ===

impl<I, S> svc::Service<I> for AuthorizeTcp<S>
where
    S: svc::Service<I, Response = ()>,
{
    type Response = ();
    type Error = S::Error;
    type Future = future::Either<S::Future, future::Ready<Result<(), S::Error>>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        match self {
            Self::Authorized(ref mut inner) => inner.poll_ready(cx),
            Self::Unauthorized => task::Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, io: I) -> Self::Future {
        match self {
            Self::Authorized(ref mut inner) => future::Either::Left(inner.call(io)),
            Self::Unauthorized => future::Either::Right(future::ok(())),
        }
    }
}
