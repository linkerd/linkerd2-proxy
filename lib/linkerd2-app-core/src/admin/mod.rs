//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.

use crate::{svc, transport::tls};
use futures::{future, Future, Poll};
use http::StatusCode;
use hyper::service::{service_fn, Service};
use hyper::{Body, Request, Response};
use linkerd2_metrics::{self as metrics, FmtMetrics};
use std::io;

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
    Box<dyn Future<Item = Response<Body>, Error = io::Error> + Send + 'static>;

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
}

impl<M: FmtMetrics> Service for Admin<M> {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = ResponseFuture;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match req.uri().path() {
            "/metrics" => Box::new(self.metrics.call(req)),
            "/proxy-log-level" => self.trace_level.call(req),
            "/ready" => Box::new(future::ok(self.ready_rsp())),
            _ => Box::new(future::ok(rsp(StatusCode::NOT_FOUND, Body::empty()))),
        }
    }
}

impl<M: FmtMetrics + Clone + Send + 'static> svc::Service<tls::Connection> for Accept<M> {
    type Response = ();
    type Error = hyper::error::Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, conn: tls::Connection) -> Self::Future {
        // Since the `/proxy-log-level` controls access based on the
        // client's IP address, we wrap the service with a new service
        // that adds the remote IP as a request extension.
        let remote = conn.remote_addr();
        let mut svc = self.0.clone();
        let svc = service_fn(move |mut req| {
            req.extensions_mut().insert(ClientAddr(remote));
            svc.call(req)
        });
        Box::new(self.1.serve_connection(conn, svc))
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
    use http::method::Method;
    use linkerd2_task::test_util::BlockOnFor;
    use std::time::Duration;
    use tokio::runtime::current_thread::Runtime;

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
                let f = srv.call(r);
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
