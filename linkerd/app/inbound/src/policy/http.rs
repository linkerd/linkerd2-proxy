use super::{RoutePolicy, Routes};
use crate::{
    http::router::RouteLabels,
    metrics::authz::HttpAuthzMetrics,
    policy::{AllowPolicy, HttpRoutePermit},
};
use futures::future;
use http_body::Body;
use linkerd_app_core::{
    metrics::RouteAuthzLabels,
    svc::{self, ServiceExt},
    tls::{self, ConditionalServerTls},
    transport::{ClientAddr, OrigDstAddr, Remote, ServerAddr},
    Conditional, Error, Result,
};
use linkerd_proxy_server_policy::{grpc, http, route::RouteMatch};
use pin_project::pin_project;
use std::{pin::Pin, sync::Arc, task};
use tokio::sync::OwnedSemaphorePermit;

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

/// A `T`-typed target with policy enforced by a [`NewHttpPolicy<N>`] layer.
#[derive(Debug)]
pub struct Permitted<T> {
    permit: HttpRoutePermit,
    protocol: PermitVariant,
    target: T,
}

#[derive(Clone, Copy, Debug)]
pub enum PermitVariant {
    Grpc,
    Http,
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

/// A response body that holds a concurrency permit until fully consumed.
/// 
/// When this body is dropped (after full consumption or connection close),
/// the permit is automatically released back to the semaphore.
#[pin_project]
pub struct ConcurrencyLimitedBody<B> {
    #[pin]
    inner: B,
    /// Held until the body is dropped (fully consumed or connection closed)
    permit: Option<OwnedSemaphorePermit>,
}

impl<B> ConcurrencyLimitedBody<B> {
    /// Wraps a body with a concurrency permit.
    pub fn new(inner: B, permit: OwnedSemaphorePermit) -> Self {
        Self {
            inner,
            permit: Some(permit),
        }
    }

    /// Wraps a body without a concurrency permit (unlimited).
    pub fn unlimited(inner: B) -> Self {
        Self {
            inner,
            permit: None,
        }
    }
}

impl<B: Body> Body for ConcurrencyLimitedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl<B: Default> Default for ConcurrencyLimitedBody<B> {
    fn default() -> Self {
        Self {
            inner: B::default(),
            permit: None,
        }
    }
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

impl<B, T, N, S, RspB> svc::Service<::http::Request<B>> for HttpPolicyService<T, N>
where
    T: Clone,
    N: svc::NewService<Permitted<T>, Service = S>,
    S: svc::Service<::http::Request<B>, Response = ::http::Response<RspB>>,
    S::Error: Into<Error>,
    RspB: Default,
{
    type Response = ::http::Response<ConcurrencyLimitedBody<RspB>>;
    type Error = Error;
    type Future = future::Either<
        ConcurrencyLimitedResponseFuture<svc::stack::Oneshot<S, ::http::Request<B>>>,
        future::Ready<Result<Self::Response>>,
    >;

    #[inline]
    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: ::http::Request<B>) -> Self::Future {
        // Find an appropriate route for the request and ensure that it's
        // authorized.
        let target = self.target.clone();
        let permit = match self.policy.routes() {
            None => err!(self.mk_route_not_found()),
            Some(Routes::Http(routes)) => {
                let (permit, mtch, route) = try_fut!(self.authorize(&routes, &req));
                try_fut!(apply_http_filters(mtch, route, &mut req));
                Permitted {
                    permit,
                    target,
                    protocol: PermitVariant::Http,
                }
            }
            Some(Routes::Grpc(routes)) => {
                let (permit, _, route) = try_fut!(self.authorize(&routes, &req));
                try_fut!(apply_grpc_filters(route, &mut req));
                Permitted {
                    permit,
                    target,
                    protocol: PermitVariant::Grpc,
                }
            }
        };

        try_fut!(self.check_rate_limit());

        // Try to acquire a concurrency permit
        let concurrency_permit = try_fut!(self.try_acquire_concurrency());

        future::Either::Left(ConcurrencyLimitedResponseFuture {
            inner: self.inner.new_service(permit).oneshot(req),
            permit: concurrency_permit,
        })
    }
}

/// A future that wraps the response body with a concurrency permit.
#[pin_project]
pub struct ConcurrencyLimitedResponseFuture<F> {
    #[pin]
    inner: F,
    permit: Option<OwnedSemaphorePermit>,
}

impl<F, RspB, E> std::future::Future for ConcurrencyLimitedResponseFuture<F>
where
    F: std::future::Future<Output = Result<::http::Response<RspB>, E>>,
    E: Into<Error>,
    RspB: Default,
{
    type Output = Result<::http::Response<ConcurrencyLimitedBody<RspB>>>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let this = self.project();
        match futures::ready!(this.inner.poll(cx)) {
            Ok(rsp) => {
                let (parts, body) = rsp.into_parts();
                let body = match this.permit.take() {
                    Some(permit) => ConcurrencyLimitedBody::new(body, permit),
                    None => ConcurrencyLimitedBody::unlimited(body),
                };
                task::Poll::Ready(Ok(::http::Response::from_parts(parts, body)))
            }
            Err(e) => task::Poll::Ready(Err(e.into())),
        }
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

        let labels = linkerd_app_core::metrics::RouteLabels {
            route: route.meta.clone(),
            server: self.policy.server_label(),
        };

        let authz = match route
            .authorizations
            .iter()
            .find(|a| super::is_authorized(a, self.connection.client, &self.connection.tls))
        {
            Some(authz) => {
                if authz.meta.is_audit() {
                    tracing::info!(
                        server.group = %labels.server.0.group(),
                        server.kind = %labels.server.0.kind(),
                        server.name = %labels.server.0.name(),
                        route.group = %labels.route.group(),
                        route.kind = %labels.route.kind(),
                        route.name = %labels.route.name(),
                        client.tls = ?self.connection.tls,
                        client.ip = %self.connection.client.ip(),
                        authz.group = %authz.meta.group(),
                        authz.kind = %authz.meta.kind(),
                        authz.name = %authz.meta.name(),
                        "Request allowed",
                    );
                }
                authz
            }
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
                self.metrics.deny(
                    labels,
                    self.connection.dst,
                    self.connection.tls.as_ref().map(|t| t.labels()),
                );
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

        self.metrics
            .allow(&permit, self.connection.tls.as_ref().map(|t| t.labels()));

        Ok((permit, r#match, route))
    }

    fn mk_route_not_found(&self) -> Error {
        let labels = self.policy.server_label();
        self.metrics.route_not_found(
            labels,
            self.connection.dst,
            self.connection.tls.as_ref().map(|t| t.labels()),
        );
        HttpRouteNotFound(()).into()
    }

    fn check_rate_limit(&self) -> Result<()> {
        let id = match self.connection.tls {
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(tls::ClientId(ref id)),
                ..
            }) => Some(id),
            _ => None,
        };
        self.policy
            .borrow()
            .local_rate_limit
            .check(id)
            .map_err(|err| {
                self.metrics.ratelimit(
                    self.policy.ratelimit_label(&err),
                    self.connection.dst,
                    self.connection.tls.as_ref().map(|t| t.labels()),
                );
                err.into()
            })
    }

    /// Try to acquire a concurrency permit.
    /// 
    /// Returns `Ok(Some(permit))` if a permit was acquired, `Ok(None)` if no
    /// concurrency limit is configured, or `Err` if the limit is exceeded.
    fn try_acquire_concurrency(&self) -> Result<Option<OwnedSemaphorePermit>> {
        self.policy
            .borrow()
            .local_concurrency_limit
            .try_acquire()
            .map(Some)
            .or_else(|err| {
                // If the error indicates no limit (default), return None
                // Otherwise, propagate the error
                if err.0 == tokio::sync::Semaphore::MAX_PERMITS {
                    Ok(None)
                } else {
                    tracing::debug!(%err, "concurrency limit exceeded");
                    // TODO: Add metrics for concurrency limit exceeded
                    Err(err.into())
                }
            })
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

// === impl Permitted ===

impl<T> svc::Param<Remote<ServerAddr>> for Permitted<T>
where
    T: svc::Param<Remote<ServerAddr>>,
{
    fn param(&self) -> Remote<ServerAddr> {
        self.target.param()
    }
}

impl<T> svc::Param<ConditionalServerTls> for Permitted<T>
where
    T: svc::Param<ConditionalServerTls>,
{
    fn param(&self) -> ConditionalServerTls {
        self.target.param()
    }
}

impl<T> svc::Param<HttpRoutePermit> for Permitted<T> {
    fn param(&self) -> HttpRoutePermit {
        self.permit_ref().clone()
    }
}

impl<T> svc::Param<PermitVariant> for Permitted<T> {
    fn param(&self) -> PermitVariant {
        self.variant()
    }
}

impl<T> svc::Param<RouteLabels> for Permitted<T> {
    fn param(&self) -> RouteLabels {
        self.route_labels()
    }
}

impl<T> Permitted<T> {
    /// Returns a reference to the [`HttpRoutePermit`] authorizing this `T`.
    pub fn permit_ref(&self) -> &HttpRoutePermit {
        &self.permit
    }

    /// Returns the [`PermitVariant`] of the permitting policy.
    pub fn variant(&self) -> PermitVariant {
        self.protocol
    }

    /// Returns a reference to the underlying `T`-typed target.
    pub fn target_ref(&self) -> &T {
        &self.target
    }

    /// Consumes this permitted `T`, returning the inner `T`.
    pub fn into_target(self) -> T {
        self.target
    }

    /// Consumes this permitted `T`, returning the `T` and its permit.
    pub fn into_parts(self) -> (T, HttpRoutePermit) {
        let Self {
            permit,
            protocol: _,
            target,
        } = self;

        (target, permit)
    }

    /// Returns the route labels from the underlying permit.
    pub fn route_labels(&self) -> RouteLabels {
        self.permit_ref().labels.route.clone().into()
    }
}
