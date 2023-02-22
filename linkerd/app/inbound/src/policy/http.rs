use super::{RoutePolicy, Routes};
use crate::{
    metrics::authz::HttpAuthzMetrics,
    policy::{AllowPolicy, HttpRoutePermit},
};
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    metrics::{RouteAuthzLabels, RouteLabels},
    svc::{self, ServiceExt},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error, Result,
};
use linkerd_proxy_server_policy::{grpc, http, route::RouteMatch};
use std::{sync::Arc, task};

#[cfg(test)]
mod tests;

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
}

#[derive(Clone, Debug)]
pub struct HttpPolicyService<T, N> {
    target: T,
    connection: ConnectionMeta,
    policy: AllowPolicy,
    metrics: HttpAuthzMetrics,
    inner: N,
}

#[derive(Clone, Debug)]
struct ConnectionMeta {
    dst: OrigDstAddr,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
}

#[derive(Debug, thiserror::Error)]
#[error("no route found for request")]
pub struct HttpRouteNotFound(());

#[derive(Debug, thiserror::Error)]
#[error("invalid redirect: {0}")]
pub struct HttpRouteInvalidRedirect(#[from] pub http::filter::InvalidRedirect);

#[derive(Debug, thiserror::Error)]
#[error("request redirected to {location}")]
pub struct HttpRouteRedirect {
    pub status: ::http::StatusCode,
    pub location: ::http::Uri,
}

#[derive(Debug, thiserror::Error)]
#[error("unauthorized request on route")]
pub struct HttpRouteUnauthorized(());

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("HTTP request configured to fail with {status}: {message}")]
pub struct HttpRouteInjectedFailure {
    pub status: ::http::StatusCode,
    pub message: Arc<str>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("gRPC request configured to fail with {code}: {message}")]
pub struct GrpcRouteInjectedFailure {
    pub code: u16,
    pub message: Arc<str>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid server policy: {0}")]
pub struct HttpInvalidPolicy(&'static str);

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
    T: svc::Param<AllowPolicy>,
    T: svc::Param<Remote<ClientAddr>>,
    T: svc::Param<tls::ConditionalServerTls>,
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
            connection: ConnectionMeta { client, dst, tls },
            metrics: self.metrics.clone(),
            inner: self.inner.clone(),
        }
    }
}

// === impl HttpPolicyService ===

macro_rules! err {
    ($e:expr) => {
        return future::Either::Right(future::err($e))
    };
}

macro_rules! try_fut {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => err!(e),
        }
    };
}

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
        future::Ready<Result<Self::Response>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: ::http::Request<B>) -> Self::Future {
        // Find an appropriate route for the request and ensure that it's
        // authorized.
        let permit = match self.policy.routes() {
            None => err!(self.mk_route_not_found()),
            Some(Routes::Http(routes)) => {
                let (permit, mtch, route) = try_fut!(self.authorize(&routes, &req));
                try_fut!(apply_http_filters(mtch, route, &mut req));
                permit
            }
            Some(Routes::Grpc(routes)) => {
                let (permit, _, route) = try_fut!(self.authorize(&routes, &req));
                try_fut!(apply_grpc_filters(route, &mut req));
                permit
            }
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
    /// Finds a matching route for the given request and checks that a
    /// sufficient authorization is present, returning a permit describing the
    /// authorization.
    fn authorize<'m, M: super::route::Match + 'm, P, B>(
        &self,
        routes: &'m [super::route::Route<M, RoutePolicy<P>>],
        req: &::http::Request<B>,
    ) -> Result<(HttpRoutePermit, RouteMatch<M::Summary>, &'m RoutePolicy<P>)> {
        let (r#match, route) =
            super::route::find(routes, req).ok_or_else(|| self.mk_route_not_found())?;

        let labels = RouteLabels {
            route: route.meta.clone(),
            server: self.policy.server_label(),
        };

        let authz = match route
            .authorizations
            .iter()
            .find(|a| super::is_authorized(a, self.connection.client, &self.connection.tls))
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
                    client.tls = ?self.connection.tls,
                    client.ip = %self.connection.client.ip(),
                    "Request denied",
                );
                if tracing::event_enabled!(tracing::Level::DEBUG) {
                    if route.authorizations.is_empty() {
                        tracing::debug!("No authorizations defined",);
                    }
                    for authz in &*route.authorizations {
                        tracing::debug!(
                            authz.group = %authz.meta.group(),
                            authz.kind = %authz.meta.kind(),
                            authz.name = %authz.meta.name(),
                            "Authorization did not apply",
                        );
                    }
                }
                self.metrics
                    .deny(labels, self.connection.dst, self.connection.tls.clone());
                return Err(HttpRouteUnauthorized(()).into());
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
                client.tls = ?self.connection.tls,
                client.ip = %self.connection.client.ip(),
                "Request authorized",
            );
            HttpRoutePermit {
                dst: self.connection.dst,
                labels,
            }
        };

        self.metrics.allow(&permit, self.connection.tls.clone());
        Ok((permit, r#match, route))
    }

    fn mk_route_not_found(&self) -> Error {
        let labels = self.policy.server_label();
        self.metrics
            .route_not_found(labels, self.connection.dst, self.connection.tls.clone());
        HttpRouteNotFound(()).into()
    }
}

fn apply_http_filters<B>(
    r#match: http::RouteMatch,
    route: &http::Policy,
    req: &mut ::http::Request<B>,
) -> Result<()> {
    // TODO Do any metrics apply here?
    for filter in &route.filters {
        match filter {
            http::Filter::InjectFailure(fail) => {
                if let Some(http::filter::FailureResponse { status, message }) = fail.apply() {
                    return Err(HttpRouteInjectedFailure { status, message }.into());
                }
            }

            http::Filter::Redirect(redir) => match redir.apply(req.uri(), &r#match) {
                Ok(Some(http::filter::Redirection { status, location })) => {
                    return Err(HttpRouteRedirect { status, location }.into());
                }

                Err(invalid) => {
                    return Err(HttpRouteInvalidRedirect(invalid).into());
                }

                Ok(None) => {
                    tracing::debug!("Ignoring irrelevant redirect");
                }
            },

            http::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            http::Filter::InternalError(msg) => {
                return Err(HttpInvalidPolicy(msg).into());
            }
        }
    }

    Ok(())
}

fn apply_grpc_filters<B>(route: &grpc::Policy, req: &mut ::http::Request<B>) -> Result<()> {
    for filter in &route.filters {
        match filter {
            grpc::Filter::InjectFailure(fail) => {
                if let Some(grpc::filter::FailureResponse { code, message }) = fail.apply() {
                    return Err(GrpcRouteInjectedFailure { code, message }.into());
                }
            }

            grpc::Filter::RequestHeaders(rh) => {
                rh.apply(req.headers_mut());
            }

            grpc::Filter::InternalError(msg) => {
                return Err(HttpInvalidPolicy(msg).into());
            }
        }
    }

    Ok(())
}
