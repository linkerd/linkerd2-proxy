//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.

use crate::{
    io,
    proxy::http::{ClientHandle, SetClientHandle},
    svc, tls, trace,
    transport::listen::Addrs,
};
use futures::future;
use http::StatusCode;
use hyper::{Body, Request, Response};
use linkerd_error::{Error, Never};
use linkerd_metrics::{self as metrics, FmtMetrics};
use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;

mod readiness;

pub use self::readiness::{Latch, Readiness};

#[derive(Clone)]
pub struct Admin<M> {
    metrics: metrics::Serve<M>,
    tracing: trace::Handle,
    ready: Readiness,
    shutdown_tx: mpsc::UnboundedSender<()>,
}

#[derive(Clone)]
pub struct Accept<M> {
    service: Admin<M>,
    server: hyper::server::conn::Http,
}

#[derive(Clone)]
pub struct Serve<M> {
    client_addr: SocketAddr,
    inner: Accept<M>,
}

pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, Never>> + Send + 'static>>;

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

    pub fn into_accept(self) -> Accept<M> {
        Accept {
            service: self,
            server: hyper::server::conn::Http::new(),
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

impl<M: FmtMetrics> tower::Service<http::Request<Body>> for Admin<M> {
    type Response = http::Response<Body>;
    type Error = Never;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match req.uri().path() {
            "/live" => Box::pin(future::ok(Self::live_rsp())),
            "/ready" => Box::pin(future::ok(self.ready_rsp())),
            "/metrics" => {
                let rsp = self.metrics.serve(req).unwrap_or_else(|error| {
                    tracing::error!(%error, "Failed to format metrics");
                    Self::internal_error_rsp(error)
                });
                Box::pin(future::ok(rsp))
            }
            "/proxy-log-level" => {
                if Self::client_is_localhost(&req) {
                    let handle = self.tracing.clone();
                    Box::pin(async move {
                        handle.serve_level(req).await.or_else(|error| {
                            tracing::error!(%error, "Failed to get/set tracing level");
                            Ok(Self::internal_error_rsp(error))
                        })
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
            path if path.starts_with("/tasks") => {
                if Self::client_is_localhost(&req) {
                    let handle = self.tracing.clone();
                    Box::pin(async move {
                        handle.serve_tasks(req).await.or_else(|error| {
                            tracing::error!(%error, "Failed to fetch tasks");
                            Ok(Self::internal_error_rsp(error))
                        })
                    })
                } else {
                    Box::pin(future::ok(Self::forbidden_not_localhost()))
                }
            }
            _ => Box::pin(future::ok(Self::not_found())),
        }
    }
}

impl<M: Clone> svc::NewService<tls::accept::Meta<Addrs>> for Accept<M> {
    type Service = Serve<M>;

    fn new_service(&mut self, (_, addrs): tls::accept::Meta<Addrs>) -> Self::Service {
        Serve {
            client_addr: addrs.peer(),
            inner: self.clone(),
        }
    }
}

impl<I, M> svc::Service<I> for Serve<M>
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    M: FmtMetrics + Clone + Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        // Since the `/proxy-log-level` controls access based on the
        // client's IP address, we wrap the service with a new service
        // that adds the remote IP as a request extension.
        let (svc, closed) = SetClientHandle::new(self.client_addr, self.inner.service.clone());
        let mut conn = self.inner.server.serve_connection(io, svc);

        Box::pin(async move {
            tokio::select! {
                res = &mut conn => res?,
                () = closed => {
                    Pin::new(&mut conn).graceful_shutdown();
                    conn.await?;
                }
            }

            Ok(())
        })
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
