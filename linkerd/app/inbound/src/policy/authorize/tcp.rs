use super::super::{AllowPolicy, DeniedUnauthorized, Permit};
use crate::metrics::authz::TcpAuthzMetrics;
use futures::future;
use linkerd_app_core::{
    svc, tls,
    transport::{ClientAddr, Remote},
    Error, Result,
};
use std::{future::Future, pin::Pin, task};

/// A middleware that enforces policy on each TCP connection. When connection is authorized, we
/// continue to monitor the policy for changes and, if the connection is no longer authorized, it is
/// dropped/closed.
///
/// Metrics are reported to the `TcpAuthzMetrics` struct.
#[derive(Clone, Debug)]
pub struct NewAuthorizeTcp<N> {
    inner: N,
    metrics: TcpAuthzMetrics,
}

#[derive(Clone, Debug)]
pub enum AuthorizeTcp<S> {
    Authorized(Authorized<S>),
    Unauthorized(Unauthorized),
}

#[derive(Clone, Debug)]
pub struct Authorized<S> {
    inner: S,
    policy: AllowPolicy,
    client: Remote<ClientAddr>,
    tls: tls::ConditionalServerTls,
    metrics: TcpAuthzMetrics,
}

#[derive(Clone, Debug)]
pub struct Unauthorized {
    deny: DeniedUnauthorized,
}

// === impl NewAuthorizeTcp ===

impl<N> NewAuthorizeTcp<N> {
    pub(crate) fn layer(
        metrics: TcpAuthzMetrics,
    ) -> impl svc::layer::Layer<N, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            metrics: metrics.clone(),
        })
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
        let client = target.param();
        let tls = target.param();
        let policy: AllowPolicy = target.param();
        match policy.check_authorized(client, &tls) {
            Ok(permit) => {
                tracing::debug!(?permit, ?tls, %client, "Connection authorized");

                // This new services requires a ClientAddr, so it must necessarily be built for each
                // connection. So we can just increment the counter here since the service can only
                // be used at most once.
                self.metrics.allow(&permit);

                let inner = self.inner.new_service((permit, target));
                AuthorizeTcp::Authorized(Authorized {
                    inner,
                    policy,
                    client,
                    tls,
                    metrics: self.metrics.clone(),
                })
            }
            Err(deny) => {
                tracing::info!(server = %policy.server_label(), ?tls, %client, "Connection denied");
                self.metrics.deny(&policy);
                AuthorizeTcp::Unauthorized(Unauthorized { deny })
            }
        }
    }
}

// === impl AuthorizeTcp ===

impl<I, S> svc::Service<I> for AuthorizeTcp<S>
where
    S: svc::Service<I, Response = ()>,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = future::Either<
        Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>,
        future::Ready<Result<()>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> task::Poll<Result<()>> {
        match self {
            Self::Authorized(Authorized { ref mut inner, .. }) => {
                inner.poll_ready(cx).map_err(Into::into)
            }

            // If connections are not authorized, fail it immediately.
            Self::Unauthorized(Unauthorized { deny }) => {
                task::Poll::Ready(Err(deny.clone().into()))
            }
        }
    }

    fn call(&mut self, io: I) -> Self::Future {
        match self {
            // If the connection is authorized, pass it to the inner service and stop processing the
            // connection if the authorization's state changes to no longer permit the request.
            Self::Authorized(Authorized {
                inner,
                client,
                tls,
                policy,
                metrics,
            }) => {
                let client = *client;
                let tls = tls.clone();
                let mut policy = policy.clone();
                let metrics = metrics.clone();

                let call = inner.call(io);
                future::Either::Left(Box::pin(async move {
                    tokio::pin!(call);
                    loop {
                        tokio::select! {
                            res = &mut call => return res.map_err(Into::into),
                            _ = policy.changed() => {
                                if let Err(denied) = policy.check_authorized(client, &tls) {
                                    tracing::info!(server = %policy.server_label(), ?tls, %client, "Connection terminated");
                                    metrics.terminate(&policy);
                                    return Err(denied.into());
                                }
                            }
                        };
                    }
                }))
            }

            Self::Unauthorized(_deny) => unreachable!("poll_ready must be called"),
        }
    }
}
