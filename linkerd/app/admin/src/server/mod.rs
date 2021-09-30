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

use futures::future;
use http::StatusCode;
use hyper::{
    body::{Body, HttpBody},
    Request, Response,
};
use linkerd_app_core::{
    metrics::{self as metrics, FmtMetrics},
    proxy::http::ClientHandle,
    trace, Error,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

mod level;
mod readiness;

pub use self::readiness::{Latch, Readiness};

#[derive(Clone)]
pub struct Admin<M> {
    metrics: metrics::Serve<M>,
    tracing: trace::Handle,
    ready: Readiness,
    shutdown_tx: mpsc::UnboundedSender<()>,
}

pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + 'static>>;

impl<M> Admin<M> {
    pub fn new(
        metrics: M,
        ready: Readiness,
        shutdown_tx: mpsc::UnboundedSender<()>,
        tracing: trace::Handle,
    ) -> Self {
        Self {
            metrics: metrics::Serve::new(metrics),
            ready,
            shutdown_tx,
            tracing,
        }
    }

    fn ready_rsp(&self) -> Response<Body> {
        if self.ready.is_ready() {
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body("ready\n".into())
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body("not ready\n".into())
                .expect("builder with known status code must not fail")
        }
    }

    fn live_rsp() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body("live\n".into())
            .expect("builder with known status code must not fail")
    }

    fn shutdown(&self) -> Response<Body> {
        if self.shutdown_tx.send(()).is_ok() {
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body("shutdown\n".into())
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body("shutdown listener dropped\n".into())
                .expect("builder with known status code must not fail")
        }
    }

    fn internal_error_rsp(error: impl ToString) -> http::Response<Body> {
        http::Response::builder()
            .status(http::StatusCode::INTERNAL_SERVER_ERROR)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(error.to_string().into())
            .expect("builder with known status code should not fail")
    }

    fn not_found() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("builder with known status code must not fail")
    }

    fn method_not_allowed() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .expect("builder with known status code must not fail")
    }

    fn forbidden_not_localhost() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::FORBIDDEN)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body("Requests are only permitted from localhost.".into())
            .expect("builder with known status code must not fail")
    }

    fn client_is_localhost<B>(req: &Request<B>) -> bool {
        req.extensions()
            .get::<ClientHandle>()
            .map(|a| a.addr.ip().is_loopback())
            .unwrap_or(false)
    }
}

impl<M, B> tower::Service<http::Request<B>> for Admin<M>
where
    M: FmtMetrics,
    B: HttpBody + Send + Sync + 'static,
    B::Error: Into<Error>,
    B::Data: Send,
{
    type Response = http::Response<Body>;
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
                if Self::client_is_localhost(&req) {
                    let level = self.tracing.level().cloned();
                    Box::pin(async move {
                        let rsp = match level {
                            Some(level) => {
                                level::serve(&level, req).await.unwrap_or_else(|error| {
                                    tracing::error!(%error, "Failed to get/set tracing level");
                                    Self::internal_error_rsp(error)
                                })
                            }
                            None => Self::not_found(),
                        };
                        Ok(rsp)
                    })
                } else {
                    Box::pin(future::ok(Self::forbidden_not_localhost()))
                }
            }
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
            _ => Box::pin(future::ok(Self::not_found())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::method::Method;
    use std::time::Duration;
    use tokio::{sync::mpsc, time::timeout};
    use tower::util::ServiceExt;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn ready_when_latches_dropped() {
        let (r, l0) = Readiness::new();
        let l1 = l0.clone();

        let (_, t) = trace::Settings::default().build();
        let (s, _) = mpsc::unbounded_channel();
        let admin = Admin::new((), r, s, t);
        macro_rules! call {
            () => {{
                let r = Request::builder()
                    .method(Method::GET)
                    .uri("http://0.0.0.0/ready")
                    .body(Body::empty())
                    .unwrap();
                let f = admin.clone().oneshot(r);
                timeout(TIMEOUT, f).await.expect("timeout").expect("call")
            };};
        }

        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        assert_eq!(call!().status(), StatusCode::OK);
    }
}
