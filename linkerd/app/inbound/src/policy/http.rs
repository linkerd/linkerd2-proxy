use crate::{
    metrics::authz::HttpAuthzMetrics,
    policy::{AllowPolicy, HttpRoutePermit},
};
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    metrics::{RouteAuthzLabels, RouteLabels, ServerLabel},
    svc::{self, ServiceExt},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error,
};
use linkerd_server_policy::{Authorization, Meta};
use std::{sync::Arc, task};

/// A middleware that enforces policy on each HTTP request.
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
    default_route_meta: Arc<Meta>,
}

#[derive(Clone, Debug)]
pub struct HttpPolicyService<T, N> {
    target: T,
    meta: ConnectionMeta,
    policy: AllowPolicy,
    metrics: HttpAuthzMetrics,
    inner: N,
    default_route_meta: Arc<Meta>,
}

#[derive(Clone, Debug)]
struct ConnectionMeta {
    dst: OrigDstAddr,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
}

#[derive(Debug, thiserror::Error)]
#[error("unauthorized request on route {}/{}", .0.kind(), .0.name())]
pub struct HttpRouteUnauthorized(Arc<Meta>);

// === impl NewHttpPolicy ===

impl<N> NewHttpPolicy<N> {
    pub fn layer(metrics: HttpAuthzMetrics) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        // XXX for now we use a single, synthetic route for all requests. This
        // will change in the near future.
        let default_route_meta = Meta::new_default("default");

        svc::layer::mk(move |inner| Self {
            metrics: metrics.clone(),
            default_route_meta: default_route_meta.clone(),
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
    type Service = HttpPolicyService<T, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let client = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        let dst = policy.dst_addr();
        HttpPolicyService {
            target,
            policy,
            meta: ConnectionMeta { client, dst, tls },
            metrics: self.metrics.clone(),
            inner: self.inner.clone(),
            default_route_meta: self.default_route_meta.clone(),
        }
    }
}

// === impl HttpPolicyService ===

impl<B, T, N, S> svc::Service<::http::Request<B>> for HttpPolicyService<T, N>
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
        future::Ready<Result<Self::Response, Error>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ::http::Request<B>) -> Self::Future {
        let server = self.policy.server.borrow();
        let labels = ServerLabel(server.meta.clone());

        let labels = RouteLabels {
            route: self.default_route_meta.clone(),
            server: labels,
        };

        let permit = match Self::check_authorized(
            &*server.authorizations,
            &self.meta,
            labels,
            &self.metrics,
        ) {
            Ok(p) => p,
            Err(deny) => return future::Either::Right(future::err(deny.into())),
        };

        future::Either::Left(
            self.inner
                .new_service((permit, self.target.clone()))
                .oneshot(req)
                .err_into::<Error>(),
        )
    }
}

impl<T, N> HttpPolicyService<T, N> {
    fn check_authorized<'a>(
        authzs: impl IntoIterator<Item = &'a Authorization>,
        conn: &ConnectionMeta,
        labels: RouteLabels,
        metrics: &HttpAuthzMetrics,
    ) -> Result<HttpRoutePermit, HttpRouteUnauthorized> {
        let authz = match authzs
            .into_iter()
            .find(|a| super::is_authorized(a, conn.client, &conn.tls))
        {
            Some(authz) => authz,
            None => {
                tracing::info!(
                    server.group = %labels.server.0.group(),
                    server.kind = %labels.server.0.kind(),
                    server.name = %labels.server.0.name(),
                    route.group = %labels.route.group(),
                    route.kind = %labels.route.kind(),
                    route.name = %labels.route.name(),
                    client.tls = ?conn.tls,
                    client.ip = %conn.client.ip(),
                    "Request denied",
                );
                let route = labels.route.clone();
                metrics.deny(labels, conn.dst, conn.tls.clone());
                return Err(HttpRouteUnauthorized(route));
            }
        };

        let permit = {
            let labels = RouteAuthzLabels {
                route: labels,
                authz: authz.meta.clone(),
            };
            tracing::debug!(
                server.group = %labels.route.server.0.group(),
                server.kind = %labels.route.server.0.kind(),
                server.name = %labels.route.server.0.name(),
                route.group = %labels.route.route.group(),
                route.kind = %labels.route.route.kind(),
                route.name = %labels.route.route.name(),
                authz.group = %labels.authz.group(),
                authz.kind = %labels.authz.kind(),
                authz.name = %labels.authz.name(),
                client.tls = ?conn.tls,
                client.ip = %conn.client.ip(),
                "Request authorized",
            );
            HttpRoutePermit {
                dst: conn.dst,
                labels,
            }
        };

        metrics.allow(&permit, conn.tls.clone());
        Ok(permit)
    }
}
