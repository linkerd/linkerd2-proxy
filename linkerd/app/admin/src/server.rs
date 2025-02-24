//! Serves an HTTP admin server.
//!
//! * `GET /metrics` -- reports prometheus-formatted metrics.
//! * `GET /ready` -- returns 200 when the proxy is ready to participate in meshed
//!   traffic.
//! * `GET /live` -- returns 200 when the proxy is live.
//! * `GET /proxy-log-level` -- returns the current proxy tracing filter.
//! * `PUT /proxy-log-level` -- sets a new tracing filter.
//! * `GET /tasks` -- returns a dump of spawned Tokio tasks (when enabled by the
//!   tracing configuration).
//! * `POST /shutdown` -- shuts down the proxy.

use futures::future::{self, TryFutureExt};
use http::StatusCode;
use linkerd_app_core::{
    metrics::{self as metrics, FmtMetrics},
    proxy::http::{Body, BoxBody, ClientHandle, Request, Response},
    trace, Error, Result,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

mod json;
mod log;
mod readiness;

pub use self::readiness::{Latch, Readiness};

#[derive(Clone)]
pub struct Admin<M> {
    metrics: metrics::Serve<M>,
    tracing: trace::Handle,
    ready: Readiness,
    shutdown_tx: mpsc::UnboundedSender<()>,
    enable_shutdown: bool,
    #[cfg(feature = "pprof")]
    pprof: Option<crate::pprof::Pprof>,
}

pub type ResponseFuture = Pin<Box<dyn Future<Output = Result<Response<BoxBody>>> + Send + 'static>>;

impl<M> Admin<M> {
    pub fn new(
        metrics: M,
        ready: Readiness,
        shutdown_tx: mpsc::UnboundedSender<()>,
        enable_shutdown: bool,
        tracing: trace::Handle,
    ) -> Self {
        Self {
            metrics: metrics::Serve::new(metrics),
            ready,
            shutdown_tx,
            enable_shutdown,
            tracing,

            #[cfg(feature = "pprof")]
            pprof: None,
        }
    }

    #[cfg(feature = "pprof")]
    pub fn with_profiling(mut self, enabled: bool) -> Self {
        self.pprof = enabled.then_some(crate::pprof::Pprof);
        self
    }

    fn ready_rsp(&self) -> Response<BoxBody> {
        if self.ready.is_ready() {
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::from_static("ready\n"))
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(BoxBody::from_static("not ready\n"))
                .expect("builder with known status code must not fail")
        }
    }

    fn live_rsp() -> Response<BoxBody> {
        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(BoxBody::from_static("live\n"))
            .expect("builder with known status code must not fail")
    }

    fn env_rsp<B>(req: Request<B>) -> Response<BoxBody> {
        use std::{collections::HashMap, env, ffi::OsString};

        if req.method() != http::Method::GET {
            return Self::method_not_allowed();
        }

        if let Err(not_acceptable) = json::accepts_json(&req) {
            return not_acceptable;
        }

        fn unicode(s: OsString) -> String {
            s.to_string_lossy().into_owned()
        }

        let query = req
            .uri()
            .path_and_query()
            .and_then(http::uri::PathAndQuery::query);
        let env = if let Some(query) = query {
            if query.contains('=') {
                return json::json_error_rsp(
                    "env.json query parameters may not contain key-value pairs",
                    StatusCode::BAD_REQUEST,
                );
            }
            query
                .split('&')
                .map(|qparam| {
                    let var = match std::env::var(qparam) {
                        Err(env::VarError::NotPresent) => None,
                        Err(env::VarError::NotUnicode(bad)) => Some(unicode(bad)),
                        Ok(var) => Some(var),
                    };
                    (qparam.to_string(), var)
                })
                .collect::<HashMap<String, Option<String>>>()
        } else {
            std::env::vars_os()
                .map(|(key, var)| (unicode(key), Some(unicode(var))))
                .collect::<HashMap<String, Option<String>>>()
        };

        json::json_rsp(&env)
    }

    fn shutdown(&self) -> Response<BoxBody> {
        if !self.enable_shutdown {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::from_static("shutdown endpoint is not enabled\n"))
                .expect("builder with known status code must not fail");
        }
        if self.shutdown_tx.send(()).is_ok() {
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::from_static("shutdown\n"))
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body(BoxBody::from_static("shutdown listener dropped\n"))
                .expect("builder with known status code must not fail")
        }
    }

    fn internal_error_rsp(error: impl ToString) -> http::Response<BoxBody> {
        http::Response::builder()
            .status(http::StatusCode::INTERNAL_SERVER_ERROR)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(BoxBody::new(error.to_string()))
            .expect("builder with known status code should not fail")
    }

    fn not_found() -> Response<BoxBody> {
        Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body(BoxBody::empty())
            .expect("builder with known status code must not fail")
    }

    fn method_not_allowed() -> Response<BoxBody> {
        Response::builder()
            .status(http::StatusCode::METHOD_NOT_ALLOWED)
            .body(BoxBody::empty())
            .expect("builder with known status code must not fail")
    }

    fn forbidden_not_localhost() -> Response<BoxBody> {
        Response::builder()
            .status(http::StatusCode::FORBIDDEN)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(BoxBody::new::<String>(
                "Requests are only permitted from localhost.".into(),
            ))
            .expect("builder with known status code must not fail")
    }

    fn client_is_localhost<B>(req: &Request<B>) -> bool {
        req.extensions()
            .get::<ClientHandle>()
            .map(|a| match a.addr.ip() {
                std::net::IpAddr::V4(v4) => v4.is_loopback(),
                std::net::IpAddr::V6(v6) => {
                    if let Some(v4) = v6.to_ipv4_mapped() {
                        v4.is_loopback()
                    } else {
                        v6.is_loopback()
                    }
                }
            })
            .unwrap_or(false)
    }
}

impl<M, B> tower::Service<http::Request<B>> for Admin<M>
where
    M: FmtMetrics,
    B: Body + Send + 'static,
    B::Error: Into<Error>,
    B::Data: Send,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        match req.uri().path() {
            "/live" => Box::pin(future::ok(Self::live_rsp())),
            "/ready" => Box::pin(future::ok(self.ready_rsp())),
            "/metrics" => {
                let rsp = self.metrics.serve(req).unwrap_or_else(|error| {
                    ::tracing::error!(%error, "Failed to format metrics");
                    Self::internal_error_rsp(error)
                });
                Box::pin(future::ok(rsp))
            }

            "/proxy-log-level" => {
                if !Self::client_is_localhost(&req) {
                    return Box::pin(future::ok(Self::forbidden_not_localhost()));
                }

                let level = match self.tracing.level() {
                    Some(level) => level.clone(),
                    None => return Box::pin(future::ok(Self::not_found())),
                };

                Box::pin(log::level::serve(level, req).or_else(|error| {
                    tracing::error!(error, "Failed to get/set tracing level");
                    future::ok(Self::internal_error_rsp(error))
                }))
            }

            #[cfg(feature = "log-streaming")]
            "/logs.json" => {
                if !Self::client_is_localhost(&req) {
                    return Box::pin(future::ok(Self::forbidden_not_localhost()));
                }

                Box::pin(
                    log::stream::serve(self.tracing.clone(), req).or_else(|error| {
                        tracing::error!(error, "Failed to stream logs");
                        future::ok(Self::internal_error_rsp(error))
                    }),
                )
            }

            "/env.json" => Box::pin(future::ok(Self::env_rsp(req))),

            "/shutdown" => {
                if req.method() == http::Method::POST {
                    if Self::client_is_localhost(&req) {
                        Box::pin(future::ok(self.shutdown()))
                    } else {
                        Box::pin(future::ok(Self::forbidden_not_localhost()))
                    }
                } else {
                    Box::pin(future::ok(Self::method_not_allowed()))
                }
            }

            #[cfg(feature = "pprof")]
            "/debug/pprof/profile.pb.gz" if self.pprof.is_some() => {
                let pprof = self.pprof.expect("unreachable");

                if !Self::client_is_localhost(&req) {
                    return Box::pin(future::ok(Self::forbidden_not_localhost()));
                }

                if req.method() != http::Method::GET {
                    return Box::pin(future::ok(Self::method_not_allowed()));
                }

                Box::pin(async move {
                    Ok(pprof
                        .profile(req)
                        .await
                        .unwrap_or_else(Self::internal_error_rsp))
                })
            }

            _ => Box::pin(future::ok(Self::not_found())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::method::Method;
    use std::time::Duration;
    use tokio::time::timeout;
    use tower::util::ServiceExt;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn ready_when_latches_dropped() {
        let (r, l0) = Readiness::new();
        let l1 = l0.clone();

        let (_, t) = trace::Settings::default().build();
        let (s, _) = mpsc::unbounded_channel();
        let admin = Admin::new((), r, s, true, t);
        macro_rules! call {
            () => {{
                let r = Request::builder()
                    .method(Method::GET)
                    .uri("http://0.0.0.0/ready")
                    .body(hyper::Body::empty())
                    .unwrap();
                let f = admin.clone().oneshot(r);
                timeout(TIMEOUT, f).await.expect("timeout").expect("call")
            }};
        }

        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        assert_eq!(call!().status(), StatusCode::OK);
    }
}
