use crate::{
    metrics::authz::HttpAuthzMetrics,
    policy::{AllowPolicy, ServerPermit},
};
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    svc::{self, ServiceExt},
    tls,
    transport::{ClientAddr, Remote},
    Error,
};
use std::task;

/// A middleware that attaches & enforces policy on each HTTP request.
///
/// This enforcement is done lazily on each request so that policy updates are
/// honored as the connection progresses.
///
/// The inner service is created for each request, so it's expected that this is
/// combined with caching.
#[derive(Clone, Debug)]
pub struct NewHttpPolicy<N> {
    metrics: HttpAuthzMetrics,
    inner: N,
}

#[derive(Clone, Debug)]
pub struct HttpPolicy<T, N> {
    target: T,
    client_addr: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
    policy: AllowPolicy,
    metrics: HttpAuthzMetrics,
    inner: N,
}

// === impl NewHttpPolicy ===

impl<N> NewHttpPolicy<N> {
    pub fn layer(metrics: HttpAuthzMetrics) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            metrics: metrics.clone(),
            inner,
        })
    }
}

impl<T, N> svc::NewService<T> for NewHttpPolicy<N>
where
    T: svc::Param<AllowPolicy>
        + svc::Param<Remote<ClientAddr>>
        + svc::Param<tls::ConditionalServerTls>,
    N: Clone,
{
    type Service = HttpPolicy<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let client_addr = target.param();
        let tls = target.param();
        let policy = target.param();
        HttpPolicy {
            target,
            client_addr,
            tls,
            policy,
            metrics: self.metrics.clone(),
            inner: self.inner.clone(),
        }
    }
}

// === impl HttpPolicy ===

impl<Req, T, N, S> svc::Service<Req> for HttpPolicy<T, N>
where
    T: Clone,
    N: svc::NewService<(ServerPermit, T), Service = S>,
    S: svc::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Either<
        future::ErrInto<svc::stack::Oneshot<S, Req>, Error>,
        future::Ready<Result<Self::Response, Error>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        tracing::trace!(policy = ?self.policy, "Authorizing request");
        match self.policy.check_authorized(self.client_addr, &self.tls) {
            Ok(permit) => {
                tracing::debug!(
                    ?permit,
                    tls = ?self.tls,
                    client = %self.client_addr,
                    "Request authorized",
                );
                self.metrics.allow(&permit, self.tls.clone());
                let svc = self.inner.new_service((permit, self.target.clone()));
                future::Either::Left(svc.oneshot(req).err_into::<Error>())
            }
            Err(e) => {
                tracing::info!(
                    server.group = %self.policy.group(),
                    server.kind = %self.policy.kind(),
                    server.name = %self.policy.name(),
                    tls = ?self.tls,
                    client = %self.client_addr,
                    "Request denied",
                );
                self.metrics.deny(&self.policy, self.tls.clone());
                future::Either::Right(future::err(e.into()))
            }
        }
    }
}
