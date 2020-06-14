//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.

use crate::{svc, transport::tls::accept::Connection};
use futures::{future, TryFutureExt};
use http::StatusCode;
use hyper::{Body, Request, Response};
use linkerd2_error::Error;
use linkerd2_metrics::{self as metrics, FmtMetrics};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{service_fn, Service};

mod readiness;
mod trace_level;

pub use self::readiness::{Latch, Readiness};
use self::trace_level::TraceLevel;

#[derive(Debug, Clone)]
pub struct Admin<M: FmtMetrics> {
    metrics: metrics::Serve<M>,
    trace_level: TraceLevel,
    ready: Readiness,
}

#[derive(Debug, Clone)]
pub struct Accept<M: FmtMetrics>(Admin<M>, hyper::server::conn::Http);

#[derive(Clone, Debug)]
pub struct ClientAddr(std::net::SocketAddr);

pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, io::Error>> + Send + 'static>>;

impl<M: FmtMetrics> Admin<M> {
    pub fn new(m: M, ready: Readiness, trace_level: TraceLevel) -> Self {
        Self {
            metrics: metrics::Serve::new(m),
            trace_level,
            ready,
        }
    }

    pub fn into_accept(self) -> Accept<M> {
        Accept(self, hyper::server::conn::Http::new())
    }

    fn ready_rsp(&self) -> Response<Body> {
        if self.ready.is_ready() {
            Response::builder()
                .status(StatusCode::OK)
                .body("ready\n".into())
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body("not ready\n".into())
                .expect("builder with known status code must not fail")
        }
    }

    fn live_rsp(&self) -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .body("live\n".into())
            .expect("builder with known status code must not fail")
    }
}

impl<M: FmtMetrics> Service<Request<Body>> for Admin<M> {
    type Response = Response<Body>;
    type Error = io::Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match req.uri().path() {
            "/metrics" => Box::pin(self.metrics.call(req)),
            "/proxy-log-level" => self.trace_level.call(req),
            "/ready" => Box::pin(future::ok(self.ready_rsp())),
            "/live" => Box::pin(future::ok(self.live_rsp())),
            _ => Box::pin(future::ok(rsp(StatusCode::NOT_FOUND, Body::empty()))),
        }
    }
}

impl<M: FmtMetrics + Clone + Send + 'static> svc::Service<Connection> for Accept<M> {
    type Response = Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'static>>;
    type Error = Error;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, (meta, io): Connection) -> Self::Future {
        // Since the `/proxy-log-level` controls access based on the
        // client's IP address, we wrap the service with a new service
        // that adds the remote IP as a request extension.
        let peer = meta.addrs.peer();
        let mut svc = self.0.clone();
        let svc = service_fn(move |mut req: Request<Body>| {
            req.extensions_mut().insert(ClientAddr(peer));
            svc.call(req)
        });

        let connection_future = self.1.serve_connection(io, svc).map_err(Into::into);
        future::ok(Box::pin(connection_future))
    }
}

impl ClientAddr {
    pub fn addr(&self) -> std::net::SocketAddr {
        self.0
    }
}

fn rsp(status: StatusCode, body: impl Into<Body>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(body.into())
        .expect("builder with known status code must not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::compat::Future01CompatExt;
    use http::method::Method;
    use linkerd2_test_util::BlockOnFor;
    use std::time::Duration;
    use tokio_compat::runtime::current_thread::Runtime;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn ready_when_latches_dropped() {
        let (r, l0) = Readiness::new();
        let l1 = l0.clone();

        let mut rt = Runtime::new().unwrap();
        let mut srv = Admin::new((), r, TraceLevel::dangling());
        macro_rules! call {
            () => {{
                let r = Request::builder()
                    .method(Method::GET)
                    .uri("http://4.3.2.1:5678/ready")
                    .body(Body::empty())
                    .unwrap();
                let f = srv.call(r).compat();
                rt.block_on_for(TIMEOUT, f).expect("call")
            };};
        }

        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        assert_eq!(call!().status(), StatusCode::OK);
    }
}
