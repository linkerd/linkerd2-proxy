use crate::policy::AllowPolicy;
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    svc::{self, ServiceExt},
    tls::{self, ClientId, ServerTls},
    Conditional, Error,
};
use linkerd_identity::Id;
use std::{sync::Arc, task};

#[derive(Clone, Debug)]
pub struct NewHttpLocalRateLimit<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct HttpLocalRateLimitService<T, N> {
    target: T,
    policy: AllowPolicy,
    tls: tls::ConditionalServerTls,
    inner: N,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("too many requests")]
pub struct RateLimitError(());


// === impl NewHttpPolicy ===

impl<N> NewHttpLocalRateLimit<N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
        })
    }
}

// === impl HttpLocalRateLimitService ===

impl<T, N> svc::NewService<T> for NewHttpLocalRateLimit<N>
where
    T: svc::Param<AllowPolicy>,
    T: svc::Param<tls::ConditionalServerTls>,
    N: Clone,
{
    type Service = HttpLocalRateLimitService<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let client = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        HttpLocalRateLimitService {
            target,
            policy,
            tls,
            inner: self.inner.clone(),
        }
    }
}

impl<B, T, N, S> svc::Service<::http::Request<B>> for HttpLocalRateLimitService<T, N>
where
    T: Clone,
    N: svc::NewService<(HttpRoutePermit, T), Service = S>,
    S: svc::Service<::http::Request<B>>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<svc::stack::Oneshot<S, ::http::Request<B>>, Error>,
        future::Ready<Result<Self::Response>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: ::http::Request<B>) -> Self::Future {
        if self.rate_limit().is_err() {
            return future::Either::Right(future::err(RateLimitError(()).into()));
        }

        future::Either::Left(
            self.inner
                .new_service((permit, self.target.clone()))
                .oneshot(req)
                .err_into::<Error>(),
        )
    }
}

impl<T, N> HttpLocalRateLimitService<T, N> {
    fn rate_limit(&self) -> std::result::Result<(), ()> {
        let arc_rl = self.policy.borrow().local_rate_limit.clone();
        let rl = Arc::clone(&arc_rl);

        if let Some(lim) = &rl.total {
            if lim.check().is_err() {
                tracing::info!("Global rate limit exceeded");
                return Err(());
            }
        }

        let client_id = self.get_client_id().unwrap_or("UNKNOWN");
        let mut has_err = false;
        let _ = &rl
            .overrides
            .iter()
            .filter(|lim| lim.ids.contains(&client_id.to_string()))
            .for_each(|lim| {
                if lim.rate_limit.check_key(&lim.ids).is_err() {
                    tracing::info!("Override rate limit exceeded");
                    has_err = true;
                }
            });

        if has_err {
            return Err(());
        }

        if let Some(lim) = &rl.identity {
            if lim.check_key(&client_id.to_string()).is_err() {
                tracing::info!("Identity rate limit exceeded");
                return Err(());
            }
        }

        Ok(())
    }

    fn get_client_id(&self) -> Option<&str> {
        match &self.connection.tls {
            Conditional::Some(ServerTls::Established {
                client_id: Some(ClientId(Id::Dns(dns_name))),
                ..
            }) => Some(dns_name.as_str()),
            _ => None,
        }
    }
}
